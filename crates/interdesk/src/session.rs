//! Source-side state machine: decides when the cursor leaves this screen,
//! where it is on the neighbour, and when it comes back. Pure logic, driven
//! by platform events, so it is tested without a display.

use std::collections::HashSet;

use crate::keymap::hid;
use crate::layout::{along, edge_at, entry_point, return_point, Rect, Side};
use crate::protocol::{Button, Msg};

/// What the platform reports.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Cursor position on this screen (only while not grabbed). `pressed`:
    /// a mouse button is held (a drag or a selection), so the cursor must
    /// not jump to another screen.
    Pos {
        x: i32,
        y: i32,
        pressed: bool,
    },
    /// Relative motion (only while grabbed).
    Delta {
        dx: i32,
        dy: i32,
    },
    Button {
        button: Button,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    Key {
        hid: u16,
        down: bool,
    },
}

/// What the runtime must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Capture (true) or release (false) the local mouse and keyboard.
    Grab(bool),
    /// Move the local cursor.
    Warp(i32, i32),
    /// Send to neighbour `index`.
    Send(usize, Msg),
}

#[derive(Debug)]
enum State {
    Local,
    Remote { idx: usize, x: i64, y: i64, w: i64, h: i64 },
}

pub struct Session {
    screen: Rect,
    sides: Vec<Side>,
    /// Part of each neighbour's edge that leads to it, as fractions.
    edges: Vec<(f64, f64)>,
    sizes: Vec<Option<(u32, u32)>>,
    state: State,
    /// Keys held on the source while remote, for the escape shortcut.
    held: HashSet<u16>,
    /// Crossing failed (input could not be captured): wait until the cursor
    /// leaves the edge before trying again.
    edge_blocked: bool,
}

impl Session {
    /// `sides[i]` is where neighbour `i` sits.
    pub fn new(screen: Rect, sides: Vec<Side>) -> Self {
        let n = sides.len();
        Self {
            screen,
            sides,
            edges: vec![(0.0, 1.0); n],
            sizes: vec![None; n],
            state: State::Local,
            held: HashSet::new(),
            edge_blocked: false,
        }
    }

    /// Neighbour `idx` faces only `from..to` of its edge (fractions).
    pub fn set_edge(&mut self, idx: usize, from: f64, to: f64) {
        if let Some(e) = self.edges.get_mut(idx) {
            *e = (from, to);
        }
    }

    pub fn remote(&self) -> Option<usize> {
        match self.state {
            State::Remote { idx, .. } => Some(idx),
            State::Local => None,
        }
    }

    pub fn set_screen(&mut self, screen: Rect) {
        self.screen = screen;
    }

    /// Neighbour `idx` is reachable with a `width`×`height` desktop. Also
    /// called when that size changes; if the cursor is there, it stays
    /// inside the new size.
    pub fn connected(&mut self, idx: usize, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if let Some(s) = self.sizes.get_mut(idx) {
            *s = Some((width, height));
        }
        if let State::Remote { idx: at, ref mut x, ref mut y, ref mut w, ref mut h } = self.state {
            if at == idx {
                (*w, *h) = (width as i64, height as i64);
                *x = (*x).clamp(0, *w - 1);
                *y = (*y).clamp(0, *h - 1);
            }
        }
    }

    pub fn disconnected(&mut self, idx: usize) -> Vec<Action> {
        if let Some(s) = self.sizes.get_mut(idx) {
            *s = None;
        }
        if self.remote() == Some(idx) {
            return self.come_back(None);
        }
        vec![]
    }

    pub fn on_input(&mut self, input: Input) -> Vec<Action> {
        match self.state {
            State::Local => self.local(input),
            State::Remote { .. } => self.remote_input(input),
        }
    }

    fn local(&mut self, input: Input) -> Vec<Action> {
        let Input::Pos { x, y, pressed: false } = input else { return vec![] };
        let Some(side) = edge_at(&self.screen, x, y) else {
            self.edge_blocked = false;
            return vec![];
        };
        if self.edge_blocked {
            return vec![];
        }
        let Some(idx) = self.sides.iter().position(|s| *s == side) else { return vec![] };
        let Some((w, h)) = self.sizes[idx] else { return vec![] };
        // Only the part of the edge the neighbour faces leads to it, mapped
        // onto the neighbour's whole edge.
        let (from, to) = self.edges[idx];
        let frac = along(&self.screen, side, x, y);
        if frac < from || frac > to {
            return vec![];
        }
        let frac = ((frac - from) / (to - from).max(f64::EPSILON)).clamp(0.0, 1.0);
        let (ex, ey) = entry_point(side, frac, w as i32, h as i32);
        self.state = State::Remote { idx, x: ex as i64, y: ey as i64, w: w as i64, h: h as i64 };
        self.held.clear();
        vec![Action::Grab(true), Action::Send(idx, Msg::Enter { x: ex, y: ey })]
    }

    fn remote_input(&mut self, input: Input) -> Vec<Action> {
        let State::Remote { idx, ref mut x, ref mut y, w, h } = self.state else { return vec![] };
        match input {
            Input::Pos { .. } => vec![],
            Input::Delta { dx, dy } => {
                let nx = *x + dx as i64;
                let ny = *y + dy as i64;
                // Crossing the edge that faces this screen brings the cursor home.
                let back = match self.sides[idx] {
                    Side::Right => nx < 0,
                    Side::Left => nx >= w,
                    Side::Bottom => ny < 0,
                    Side::Top => ny >= h,
                };
                if back {
                    let frac = match self.sides[idx] {
                        Side::Left | Side::Right => ny as f64 / (h - 1).max(1) as f64,
                        Side::Top | Side::Bottom => nx as f64 / (w - 1).max(1) as f64,
                    };
                    return self.come_back(Some(frac.clamp(0.0, 1.0)));
                }
                *x = nx.clamp(0, w - 1);
                *y = ny.clamp(0, h - 1);
                vec![Action::Send(idx, Msg::Move { x: *x as i32, y: *y as i32 })]
            }
            Input::Button { button, down } => vec![Action::Send(idx, Msg::Button { button, down })],
            Input::Wheel { dx, dy } => vec![Action::Send(idx, Msg::Wheel { dx, dy })],
            Input::Key { hid: key, down } => {
                if down {
                    self.held.insert(key);
                } else {
                    self.held.remove(&key);
                }
                let ctrl = self.held.contains(&hid::LEFT_CTRL) || self.held.contains(&hid::RIGHT_CTRL);
                let alt = self.held.contains(&hid::LEFT_ALT) || self.held.contains(&hid::RIGHT_ALT);
                if down && key == hid::ESCAPE && ctrl && alt {
                    // Emergency exit: take control back right now.
                    return self.come_back(None);
                }
                vec![Action::Send(idx, Msg::Key { hid: key, down })]
            }
        }
    }

    /// The crossing just decided could not happen (the platform failed to
    /// capture input): back to local, before anything was sent to the
    /// neighbour. The edge stays closed until the cursor moves away from it.
    /// Neighbour `idx` is driven by another computer: come back as if the
    /// link had dropped, and keep that edge closed until the cursor leaves it.
    pub fn refused(&mut self, idx: usize) -> Vec<Action> {
        if self.remote() != Some(idx) {
            return vec![];
        }
        let actions = self.come_back(None);
        self.edge_blocked = true;
        actions
    }

    pub fn cancel_crossing(&mut self) {
        if matches!(self.state, State::Remote { .. }) {
            self.state = State::Local;
            self.held.clear();
            self.edge_blocked = true;
        }
    }

    /// Returns control to this screen. `frac` = where along the shared edge;
    /// `None` = centre of the screen.
    fn come_back(&mut self, frac: Option<f64>) -> Vec<Action> {
        let State::Remote { idx, .. } = self.state else { return vec![] };
        self.state = State::Local;
        self.held.clear();
        let (wx, wy) = match frac {
            Some(f) => {
                let (from, to) = self.edges[idx];
                return_point(&self.screen, self.sides[idx], from + f * (to - from))
            }
            None => self.screen.center(),
        };
        vec![Action::Send(idx, Msg::Leave), Action::Grab(false), Action::Warp(wx, wy)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let mut s = Session::new(Rect { x: 0, y: 0, w: 1920, h: 1080 }, vec![Side::Right, Side::Top]);
        s.connected(0, 2560, 1440);
        s
    }

    #[test]
    fn crossing_and_coming_back() {
        let mut s = session();
        assert!(s.on_input(Input::Pos { x: 1000, y: 540, pressed: false }).is_empty());
        let a = s.on_input(Input::Pos { x: 1919, y: 540, pressed: false });
        assert_eq!(a, vec![Action::Grab(true), Action::Send(0, Msg::Enter { x: 0, y: 720 })]);
        assert_eq!(s.remote(), Some(0));

        assert_eq!(s.on_input(Input::Delta { dx: 30, dy: -20 }), vec![Action::Send(0, Msg::Move { x: 30, y: 700 })]);
        // Clamped at the far edge, no crossing there.
        assert_eq!(s.on_input(Input::Delta { dx: 9000, dy: 0 }), vec![Action::Send(0, Msg::Move { x: 2559, y: 700 })]);
        s.on_input(Input::Delta { dx: -2559, dy: 0 });
        let a = s.on_input(Input::Delta { dx: -1, dy: 0 });
        assert_eq!(a[0], Action::Send(0, Msg::Leave));
        assert_eq!(a[1], Action::Grab(false));
        assert_eq!(a[2], Action::Warp(1917, 525)); // 700 / 1439 of 1080
        assert_eq!(s.remote(), None);
    }

    #[test]
    fn unconnected_or_unconfigured_edges_do_nothing() {
        let mut s = session();
        assert!(s.on_input(Input::Pos { x: 0, y: 500, pressed: false }).is_empty()); // nobody on the left
        assert!(s.on_input(Input::Pos { x: 500, y: 0, pressed: false }).is_empty());
        // top neighbour offline
    }

    #[test]
    fn events_are_forwarded_and_escape_shortcut_works() {
        let mut s = session();
        s.on_input(Input::Pos { x: 1919, y: 0, pressed: false });
        let click = Input::Button { button: Button::Left, down: true };
        assert_eq!(s.on_input(click), vec![Action::Send(0, Msg::Button { button: Button::Left, down: true })]);
        assert_eq!(s.on_input(Input::Wheel { dx: 0, dy: -120 }), vec![Action::Send(0, Msg::Wheel { dx: 0, dy: -120 })]);
        s.on_input(Input::Key { hid: hid::LEFT_CTRL, down: true });
        s.on_input(Input::Key { hid: hid::LEFT_ALT, down: true });
        let a = s.on_input(Input::Key { hid: hid::ESCAPE, down: true });
        assert_eq!(a, vec![Action::Send(0, Msg::Leave), Action::Grab(false), Action::Warp(960, 540)]);
    }

    #[test]
    fn losing_the_neighbour_releases_the_grab() {
        let mut s = session();
        s.on_input(Input::Pos { x: 1919, y: 100, pressed: false });
        let a = s.disconnected(0);
        assert_eq!(a, vec![Action::Send(0, Msg::Leave), Action::Grab(false), Action::Warp(960, 540)]);
        assert!(s.on_input(Input::Pos { x: 1919, y: 100, pressed: false }).is_empty());
    }

    #[test]
    fn a_failed_grab_keeps_the_cursor_here_until_it_leaves_the_edge() {
        let mut s = session();
        assert_eq!(s.on_input(Input::Pos { x: 1919, y: 540, pressed: false })[0], Action::Grab(true));
        s.cancel_crossing();
        assert_eq!(s.remote(), None);
        // Still at the edge: no new attempt on every move.
        assert!(s.on_input(Input::Pos { x: 1919, y: 541, pressed: false }).is_empty());
        // Away from the edge and back: try again.
        assert!(s.on_input(Input::Pos { x: 1800, y: 541, pressed: false }).is_empty());
        assert_eq!(s.on_input(Input::Pos { x: 1919, y: 541, pressed: false })[0], Action::Grab(true));
    }

    #[test]
    fn a_neighbour_that_shrinks_keeps_the_cursor_inside() {
        let mut s = session();
        s.on_input(Input::Pos { x: 1919, y: 540, pressed: false });
        s.on_input(Input::Delta { dx: 2000, dy: 600 }); // at (2000, 1320) on 2560×1440
        s.connected(0, 1280, 720);
        assert_eq!(s.on_input(Input::Delta { dx: 1, dy: 0 }), vec![Action::Send(0, Msg::Move { x: 1279, y: 719 })]);
    }

    #[test]
    fn a_busy_neighbour_sends_the_cursor_back() {
        let mut s = session();
        s.on_input(Input::Pos { x: 1919, y: 540, pressed: false });
        let a = s.refused(0);
        assert_eq!(a, vec![Action::Send(0, Msg::Leave), Action::Grab(false), Action::Warp(960, 540)]);
        assert!(s.on_input(Input::Pos { x: 1919, y: 540, pressed: false }).is_empty(), "edge closed");
        s.on_input(Input::Pos { x: 1000, y: 540, pressed: false });
        assert_eq!(s.on_input(Input::Pos { x: 1919, y: 540, pressed: false })[0], Action::Grab(true));
    }

    #[test]
    fn a_smaller_neighbour_faces_part_of_the_edge() {
        // 2560×1440 neighbour on the lower half of the right edge (y 540..1079).
        let mut s = session();
        s.set_edge(0, 0.5, 1.0);
        assert!(s.on_input(Input::Pos { x: 1919, y: 200, pressed: false }).is_empty(), "upper half leads nowhere");
        let a = s.on_input(Input::Pos { x: 1919, y: 540, pressed: false });
        let Action::Send(0, Msg::Enter { x: 0, y }) = a[1] else { panic!("{a:?}") };
        assert!(y <= 1, "top of the range = top of the neighbour, got y = {y}");
        // Coming back from the neighbour's bottom lands at the bottom of the range.
        s.on_input(Input::Delta { dx: 0, dy: 1439 });
        let back = s.on_input(Input::Delta { dx: -1, dy: 0 });
        assert!(matches!(back.last(), Some(Action::Warp(_, 1079))), "{back:?}");
    }

    #[test]
    fn no_crossing_while_dragging() {
        let mut s = session();
        assert!(s.on_input(Input::Pos { x: 1919, y: 540, pressed: true }).is_empty());
        assert_eq!(s.remote(), None);
    }

    #[test]
    fn neighbour_above() {
        let mut s = session();
        s.connected(1, 1280, 800);
        let a = s.on_input(Input::Pos { x: 960, y: 0, pressed: false });
        assert_eq!(a[1], Action::Send(1, Msg::Enter { x: 640, y: 799 }));
        let a = s.on_input(Input::Delta { dx: 0, dy: 1 });
        assert_eq!(a[0], Action::Send(1, Msg::Leave));
        assert_eq!(a[2], Action::Warp(960, 2));
    }
}
