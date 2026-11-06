//! Every checkout open in this run, and which of them is in front.
//!
//! One value rather than a list and a separate index: the front is a position
//! in that list, and apart they can disagree — a removal that moved one and not
//! the other left the next turn in somebody else's lane.

use super::lane::Lane;

pub struct Lanes {
    open: Vec<Lane>,
    front: usize,
}

impl Lanes {
    /// A run that starts in `lane`, which is in front.
    pub fn of(lane: Lane) -> Self {
        Self {
            open: vec![lane],
            front: 0,
        }
    }

    /// The lanes, in the order they were opened.
    pub fn all(&self) -> &[Lane] {
        &self.open
    }

    /// The same, mutable; a slice cannot reorder, add or drop one.
    pub fn all_mut(&mut self) -> &mut [Lane] {
        &mut self.open
    }

    /// Where the front lane stands in `all`.
    pub fn front(&self) -> usize {
        self.front
    }

    /// The lane in front: what a surface shows and a typed line answers to.
    pub fn ahead(&self) -> &Lane {
        &self.open[self.front]
    }

    pub fn ahead_mut(&mut self) -> &mut Lane {
        &mut self.open[self.front]
    }

    /// The lane at `at`, for a position from `front`, `position_of` or `all`.
    pub fn at(&self, at: usize) -> &Lane {
        &self.open[at]
    }

    pub fn at_mut(&mut self, at: usize) -> &mut Lane {
        &mut self.open[at]
    }

    /// Where the lane `token` names sits now; `None` once it is gone.
    pub fn position_of(&self, token: u64) -> Option<usize> {
        self.open.iter().position(|lane| lane.token() == token)
    }

    /// Add a lane behind the others, leaving the front where it was.
    pub fn push(&mut self, lane: Lane) -> usize {
        self.open.push(lane);
        self.open.len() - 1
    }

    /// Add a lane and put it in front: what opening a checkout does.
    pub fn push_ahead(&mut self, lane: Lane) {
        let at = self.push(lane);
        self.front = at;
    }

    /// Put a lane at `at`, moving the front down with it when it lands ahead
    /// of the one in front.
    pub fn insert(&mut self, at: usize, lane: Lane) {
        self.open.insert(at, lane);
        if at <= self.front {
            self.front += 1;
        }
    }

    /// Put the lane at `at` in front.
    pub fn show(&mut self, at: usize) {
        self.front = at;
    }

    /// Reorder around the lane `front`, which stays in front: a restart brings
    /// the checkouts back in one order and the front one back to the same place.
    pub fn sort_keeping<K: Ord>(&mut self, front: u64, mut key: impl FnMut(&Lane) -> K) {
        self.open.sort_by_cached_key(|lane| key(lane));
        self.front = self.position_of(front).unwrap_or(0);
    }

    /// Take a lane out by its token, and answer it so the caller can say what
    /// went with it.
    ///
    /// The front lane stays: a run always has one in front, and `ahead()` names
    /// it. A token that is gone, or that is in front, answers `None`.
    pub fn take(&mut self, token: u64) -> Option<Lane> {
        let at = self.position_of(token)?;
        if at == self.front {
            return None;
        }
        let lane = self.open.remove(at);
        // Only the positions after it moved; the front keeps naming the lane
        // it named.
        if at < self.front {
            self.front -= 1;
        }
        Some(lane)
    }
}

impl core::ops::Deref for Lanes {
    type Target = [Lane];

    fn deref(&self) -> &[Lane] {
        &self.open
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::a_lane;

    /// Three lanes, the middle one in front — the shape a removal has to keep
    /// straight.
    fn three() -> (Lanes, [u64; 3]) {
        let mut lanes = Lanes::of(a_lane("first"));
        let a = lanes.ahead().token();
        let b = lanes.push(a_lane("second"));
        let b = lanes.at(b).token();
        let c = lanes.push(a_lane("third"));
        let c = lanes.at(c).token();
        lanes.show(1);
        (lanes, [a, b, c])
    }

    #[test]
    fn the_front_keeps_naming_the_lane_it_named() {
        let (mut lanes, [a, b, c]) = three();
        // One taken out ahead of it: the front moves down with the lane that
        // went, and names the same lane it named.
        lanes.take(a).expect("the first lane goes");
        assert_eq!(lanes.ahead().token(), b);
        assert_eq!(lanes.front(), 0);
        // One taken out behind it: the front does not move at all.
        lanes.take(c).expect("the last lane goes");
        assert_eq!(lanes.ahead().token(), b);
        assert_eq!(lanes.front(), 0);
        assert_eq!(lanes.all().len(), 1);
    }

    #[test]
    fn the_lane_in_front_is_never_taken_out() {
        let (mut lanes, [_, b, _]) = three();
        assert!(lanes.take(b).is_none(), "the front lane went");
        assert_eq!(lanes.all().len(), 3);
    }

    #[test]
    fn a_token_that_is_gone_answers_none() {
        let (mut lanes, [a, _, _]) = three();
        lanes.take(a).expect("the first lane goes");
        assert!(lanes.take(a).is_none(), "a gone token was taken out twice");
        assert!(lanes.take(0).is_none(), "a token no lane holds");
    }

    #[test]
    fn a_reorder_keeps_the_front_lane_in_front() {
        let (mut lanes, [_, b, _]) = three();
        let before = lanes.all().len();
        lanes.sort_keeping(b, |lane| std::cmp::Reverse(lane.token()));
        assert_eq!(lanes.ahead().token(), b);
        assert_eq!(lanes.all().len(), before);
    }
}
