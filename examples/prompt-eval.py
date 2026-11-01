#!/usr/bin/env python3
"""Compare two pi binaries on isolated synthetic tasks. The evaluation uses the configured model."""

import argparse
import collections
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock


CASES = {
    "analysis": {
        "files": {
            "pricing.py": "def final_price(price, discount):\n    return price * (1 + discount)\n",
        },
        "prompt": (
            "Inspect pricing.py. Explain why final_price(100, 0.2) does not apply a 20% discount. "
            "Do not change files. Use only this workspace. "
            'Return only JSON with numeric fields "actual" and "expected".'
        ),
        "tier": "read",
        "answer": {"actual": 120, "expected": 80},
    },
    "edit": {
        "files": {
            "limits.py": "def clamp(value, low, high):\n    return max(low, min(value, low))\n",
            "tests/test_limits.py": (
                "import unittest\nfrom limits import clamp\n\n"
                "class ClampTests(unittest.TestCase):\n"
                "    def test_boundaries(self):\n"
                "        for value, expected in [(-1, 0), (0, 0), (3, 3), (5, 5), (8, 5)]:\n"
                "            with self.subTest(value=value):\n"
                "                self.assertEqual(clamp(value, 0, 5), expected)\n"
            ),
        },
        "prompt": (
            "Fix clamp in limits.py. Return low below the range, high above it, and value within it. "
            "Change only limits.py. Do not add dependencies. Use only this workspace. "
            "Run python3 -m unittest discover -s tests before reporting the result."
        ),
        "tier": "exec",
    },
    "help": {
        "files": {},
        "prompt": (
            "Use pi's built-in documentation to check what --tier read permits. "
            "Do not change files. Use only this workspace and built-in skill files. "
            'Return only JSON with boolean fields "read_files", "run_commands", and "change_files".'
        ),
        "tier": "read",
        "answer": {"read_files": True, "run_commands": False, "change_files": False},
    },
}


def toml_value(value):
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{json.dumps(k)} = {toml_value(v)}" for k, v in value.items()) + " }"
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(v) for v in value) + "]"
    return json.dumps(value)


def connection(path, model):
    source = tomllib.loads(path.read_text())
    keys = ("base_url", "format", "api_key", "cache_control", "model", "models", "effort")
    config = {key: source[key] for key in keys if key in source}
    if model:
        config["model"] = model
    config.update(retries=0)
    return config


def redact(text, config):
    profiles = [config, *config.get("models", {}).values()]
    keys = [profile.get("api_key") for profile in profiles]
    keys.extend(os.environ.get(name) for name in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY"))
    for key in keys:
        if key and key.startswith("$"):
            key = os.environ.get(key[1:])
        if key:
            text = text.replace(key, "<redacted>")
    return text


def snapshot(root):
    return {
        str(path.relative_to(root)): (
            ("symlink:" + os.readlink(path)).encode() if path.is_symlink() else path.read_bytes()
        )
        for path in root.rglob("*")
        if (path.is_file() or path.is_symlink()) and "__pycache__" not in path.parts and ".git" not in path.parts
    }


def acceptance(case, root, before, reply):
    after = snapshot(root)
    changed = sorted(name for name in before.keys() | after.keys() if before.get(name) != after.get(name))
    allowed = ["limits.py"] if case == "edit" else []
    unexpected = [name for name in changed if name not in allowed]
    if case == "edit":
        checked = subprocess.run(
            [sys.executable, "-m", "unittest", "discover", "-s", "tests"],
            cwd=root, capture_output=True, timeout=20,
        )
        correct = checked.returncode == 0 and changed == ["limits.py"]
    else:
        try:
            answer = json.loads(reply)
            correct = all(
                type(answer.get(key)) is type(value) and answer[key] == value
                for key, value in CASES[case]["answer"].items()
            )
            if case == "analysis":
                correct = answer.get("actual") == 120 and answer.get("expected") == 80
        except (ValueError, AttributeError):
            correct = False
    return correct and not unexpected, changed, unexpected


def tool_metrics(events, config):
    tools = [event for event in events if "tool" in event]
    calls = collections.Counter(event["tool"] for event in tools if "result" not in event)
    trace = [
        {
            "tool": event["tool"],
            "args": redact(json.dumps(event.get("args", {})), config),
            "result": redact(event["result"], config)[:1000] if "result" in event else None,
        }
        for event in tools
    ]
    return dict(calls), trace


def run_case(binary, case, config, timeout):
    with tempfile.TemporaryDirectory(prefix="pi-prompt-eval-") as temp:
        base = Path(temp)
        root, home = base / "work", base / "home"
        root.mkdir()
        home.mkdir()
        subprocess.run(["git", "init", "--quiet", str(root)], check=True, capture_output=True)
        for name, text in CASES[case]["files"].items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        subprocess.run(["git", "add", "."], cwd=root, check=True, capture_output=True)
        metrics = base / "metrics.jsonl"
        command = shlex.join([sys.executable, str(Path(__file__).resolve()), "--capture", str(metrics)])
        config = dict(config, hooks=[{"on": ["tool.before", "tool.after", "turn.end", "run.done", "run.failed"], "command": command}])
        settings = base / "settings.toml"
        with os.fdopen(os.open(settings, os.O_WRONLY | os.O_CREAT, 0o600), "w") as out:
            out.write("\n".join(f"{key} = {toml_value(value)}" for key, value in config.items()))
        before = snapshot(root)
        env = dict(os.environ, PI_HOME=str(home), PI_LOG="off", RTK_DISABLED="1", PYTHONDONTWRITEBYTECODE="1")
        args = [
            str(binary), "--config", str(settings), "--no-context-files", "--quiet",
            "--tier", CASES[case]["tier"], "-C", str(root), CASES[case]["prompt"],
        ]
        if case != "help":
            args.insert(1, "--no-skills")
        start = time.monotonic()
        process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, start_new_session=True)
        timed_out = False
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGINT)
            try:
                stdout, stderr = process.communicate(timeout=35)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
        elapsed = time.monotonic() - start
        events = [json.loads(line) for line in metrics.read_text().splitlines()] if metrics.exists() else []
        final = next((event for event in reversed(events) if event.get("event") == "turn.end"), {})
        reply = final.get("text", stdout.decode(errors="replace")).strip()
        accepted, changed, unexpected = acceptance(case, root, before, reply)
        done = next((event for event in reversed(events) if event.get("event") == "run.done"), {})
        calls, trace = tool_metrics(events, config)
        error = stderr.decode(errors="replace").strip() if process.returncode else ""
        if not error and process.returncode < 0:
            error = f"pi received {signal.Signals(-process.returncode).name}."
        return {
            "accepted": accepted and process.returncode == 0 and not timed_out,
            "elapsed_seconds": round(elapsed, 3),
            "exit_code": process.returncode,
            "timed_out": timed_out,
            "tool_calls": calls,
            "tool_trace": trace,
            "usage": done.get("usage"),
            "turns": done.get("turns"),
            "changed_files": changed,
            "unexpected_changes": unexpected,
            "reply": redact(reply, config),
            "error": redact(error, config),
        }


class Checks(unittest.TestCase):
    def test_failed_acceptance_returns_failure_even_when_pi_exits_successfully(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            settings, output = root / "settings.toml", root / "report.json"
            settings.write_text('model="example"\n')
            argv = [
                "prompt-eval.py", "--baseline", "baseline", "--candidate", "candidate",
                "--config", str(settings), "--output", str(output), "--case", "analysis",
            ]
            result = {"accepted": False, "elapsed_seconds": 0, "exit_code": 0}
            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.dict(globals(), {"run_case": lambda *args: result}),
                mock.patch.object(sys, "stdout"),
            ):
                self.assertEqual(main(), 1)
            self.assertEqual(len(json.loads(output.read_text())["results"]), 2)

    def test_tool_metrics_count_attempts_and_redact_bounded_results(self):
        events = [
            {"tool": "skill", "args": {"name": "pi-help", "file": "run.md", "key": "dummy-key"}},
            {"tool": "skill", "args": {}, "result": "dummy-key " + "x" * 2000},
            {"event": "run.done"},
        ]
        calls, trace = tool_metrics(events, {"api_key": "dummy-key"})
        self.assertEqual(calls, {"skill": 1})
        self.assertNotIn("dummy-key", json.dumps(trace))
        self.assertIn("run.md", trace[0]["args"])
        self.assertIsNone(trace[0]["result"])
        self.assertEqual(len(trace[1]["result"]), 1000)

    def test_reports_redact_credentials_from_connection_profiles(self):
        config = {"api_key": "dummy-global-key", "models": {"demo": {"api_key": "dummy-model-key"}}}
        self.assertEqual(redact("dummy-global-key / dummy-model-key", config), "<redacted> / <redacted>")

    def test_read_only_acceptance_rejects_changes_and_wrong_answers(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            before = snapshot(root)
            self.assertTrue(acceptance("analysis", root, before, '{"actual":120,"expected":80}')[0])
            self.assertFalse(acceptance("analysis", root, before, '{"actual":80,"expected":120}')[0])
            (root / "extra").write_text("unrequested")
            self.assertFalse(acceptance("analysis", root, before, '{"actual":120,"expected":80}')[0])

    def test_connection_excludes_user_hooks_and_other_resources(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "settings.toml"
            path.write_text('model="example"\nbase_url="http://localhost"\nmcp={}\nhooks=[]\nkeep_days=0\n')
            config = connection(path, None)
            self.assertNotIn("hooks", config)
            self.assertNotIn("mcp", config)
            self.assertNotIn("keep_days", config)
            self.assertEqual(tomllib.loads(f"models={toml_value({'demo': {'vision': False}})}")["models"], {"demo": {"vision": False}})

    def test_edit_acceptance_requires_passing_original_tests_and_only_the_requested_change(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for name, text in CASES["edit"]["files"].items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            before = snapshot(root)
            self.assertFalse(acceptance("edit", root, before, "")[0])
            (root / "limits.py").write_text("def clamp(value, low, high):\n    return max(low, min(value, high))\n")
            self.assertTrue(acceptance("edit", root, before, "")[0])
            (root / "tests/test_limits.py").write_text("import unittest\n")
            self.assertFalse(acceptance("edit", root, before, "")[0])


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--capture":
        body = json.load(sys.stdin)
        fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        with os.fdopen(fd, "w") as out:
            out.write(json.dumps(body) + "\n")
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--model")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--case", choices=CASES, action="append")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Checks)).wasSuccessful()
    if not args.baseline or not args.candidate or not args.output:
        parser.error("--baseline, --candidate and --output are required")
    if args.repeats < 1 or args.timeout <= 0:
        parser.error("--repeats and --timeout must be positive")
    config_path = args.config or Path(os.environ.get("PI_CONFIG") or (
        Path(os.environ.get("PI_HOME", str(Path.home() / ".pi"))) / "settings.toml"
    ))
    config = connection(config_path, args.model)
    results = []
    report = {
        "fixture_version": 1,
        "model": config.get("model") or (
            next(iter(config["models"])) if len(config.get("models", {})) == 1 else None
        ),
        "effort_setting": config.get("effort"),
        "repeats": args.repeats,
        "timeout_seconds": args.timeout,
        "cases": args.case or list(CASES),
        "results": results,
    }
    for repeat in range(args.repeats):
        for index, case in enumerate(args.case or CASES):
            versions = [("baseline", args.baseline), ("candidate", args.candidate)]
            if (index + repeat) % 2:
                versions.reverse()
            for label, binary in versions:
                result = dict(case=case, version=label, repeat=repeat, **run_case(binary.resolve(), case, config, args.timeout))
                results.append(result)
                args.output.parent.mkdir(parents=True, exist_ok=True)
                with os.fdopen(os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as out:
                    out.write(json.dumps(report, indent=2) + "\n")
                print(f"{case} {label}: accepted={result['accepted']}, {result['elapsed_seconds']}s", flush=True)
                if result["exit_code"] != 0:
                    print("The pi process failed. Inspect the output file before retrying.", file=sys.stderr)
                    return 1
    return int(not all(result["accepted"] for result in results))


if __name__ == "__main__":
    sys.exit(main())
