//! Focus cycling over the host and its clients.

/// Where keyboard/mouse input goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target<P> {
    /// The host itself (its local passthrough device).
    Host,
    Client(P),
}

/// Focus targets in cycling order: the host, then each available client in
/// configured order, then back to the host.
///
/// Focus is tracked by identity, not position, so clients joining or
/// leaving never shift focus to a different machine. Clients that aren't
/// available (not connected, or not ready to play input) are skipped. If
/// the focused client becomes unavailable, focus returns to the host:
/// input lands on the machine you're sitting at rather than on another
/// client. (NKMS moved it to the next client in the list instead.)
#[derive(Debug, Clone)]
pub struct FocusRing<P> {
    /// Every configured client in order, with whether it's available.
    clients: Vec<(P, bool)>,
    focused: Target<P>,
}

/// A focus request for a client that isn't configured or isn't available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("that client is not connected")]
pub struct Unavailable;

impl<P: Copy + Eq> FocusRing<P> {
    /// A ring over `clients` (in cycling order), none of them available yet.
    /// Duplicates are ignored.
    pub fn new(clients: impl IntoIterator<Item = P>) -> Self {
        let mut list: Vec<(P, bool)> = Vec::new();
        for p in clients {
            if !list.iter().any(|&(q, _)| q == p) {
                list.push((p, false));
            }
        }
        Self {
            clients: list,
            focused: Target::Host,
        }
    }

    pub fn focused(&self) -> Target<P> {
        self.focused
    }

    pub fn is_available(&self, client: P) -> bool {
        self.clients.iter().any(|&(p, up)| p == client && up)
    }

    /// Every configured client, in cycling order.
    pub fn clients(&self) -> impl Iterator<Item = P> + '_ {
        self.clients.iter().map(|&(p, _)| p)
    }

    /// Available clients, in cycling order.
    pub fn available(&self) -> impl Iterator<Item = P> + '_ {
        self.clients.iter().filter(|&&(_, up)| up).map(|&(p, _)| p)
    }

    /// Marks a configured client available or not. Returns the new focus if
    /// this moved it (only when the focused client becomes unavailable).
    /// Unknown clients are ignored.
    pub fn set_available(&mut self, client: P, available: bool) -> Option<Target<P>> {
        let entry = self.clients.iter_mut().find(|(p, _)| *p == client)?;
        entry.1 = available;
        if !available && self.focused == Target::Client(client) {
            self.focused = Target::Host;
            return Some(Target::Host);
        }
        None
    }

    /// Replaces the configured clients (e.g. after a config reload), in
    /// their new cycling order. Clients kept keep their availability; new
    /// ones start unavailable. Returns the new focus if this moved it
    /// (only when the focused client was removed: focus returns to the
    /// host).
    pub fn set_clients(&mut self, clients: impl IntoIterator<Item = P>) -> Option<Target<P>> {
        let mut list: Vec<(P, bool)> = Vec::new();
        for p in clients {
            if !list.iter().any(|&(q, _)| q == p) {
                let up = self.is_available(p);
                list.push((p, up));
            }
        }
        self.clients = list;
        match self.focused {
            Target::Client(c) if !self.is_available(c) => self.set(Target::Host),
            _ => None,
        }
    }

    /// Advances focus to the next target in the cycle, wrapping back to the
    /// host. Returns the new focus if it changed (with no clients available,
    /// it stays on the host).
    pub fn advance(&mut self) -> Option<Target<P>> {
        let start = match self.focused {
            Target::Host => 0,
            Target::Client(c) => {
                // Focus is always on an available client, so it's present.
                self.clients
                    .iter()
                    .position(|&(p, _)| p == c)
                    .map_or(self.clients.len(), |i| i + 1)
            }
        };
        let next = self.clients[start.min(self.clients.len())..]
            .iter()
            .find(|&&(_, up)| up)
            .map_or(Target::Host, |&(p, _)| Target::Client(p));
        self.set(next)
    }

    /// Focuses a specific target. Returns the new focus if it changed.
    pub fn focus(&mut self, target: Target<P>) -> Result<Option<Target<P>>, Unavailable> {
        if let Target::Client(c) = target
            && !self.is_available(c)
        {
            return Err(Unavailable);
        }
        Ok(self.set(target))
    }

    fn set(&mut self, target: Target<P>) -> Option<Target<P>> {
        (self.focused != target).then(|| {
            self.focused = target;
            target
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Target::{Client, Host};

    fn ring(up: &[char]) -> FocusRing<char> {
        let mut r = FocusRing::new(['a', 'b', 'c']);
        for &c in up {
            r.set_available(c, true);
        }
        r
    }

    fn cycle(r: &mut FocusRing<char>, n: usize) -> Vec<Target<char>> {
        (0..n)
            .map(|_| {
                r.advance();
                r.focused()
            })
            .collect()
    }

    #[test]
    fn cycles_host_through_clients_and_back() {
        let mut r = ring(&['a', 'b', 'c']);
        assert_eq!(r.focused(), Host);
        assert_eq!(
            cycle(&mut r, 4),
            [Client('a'), Client('b'), Client('c'), Host]
        );
    }

    #[test]
    fn skips_unavailable_clients() {
        let mut r = ring(&['a', 'c']);
        assert_eq!(cycle(&mut r, 3), [Client('a'), Client('c'), Host]);
    }

    #[test]
    fn set_clients_keeps_availability_and_focus_by_identity() {
        let mut r = ring(&['a', 'c']);
        r.advance();
        r.advance();
        assert_eq!(r.focused(), Client('c'));
        // Reordered, 'b' dropped, 'd' added: focus stays on 'c', and the
        // cycle follows the new order.
        assert_eq!(r.set_clients(['c', 'd', 'a']), None);
        assert_eq!(r.focused(), Client('c'));
        assert!(!r.is_available('d'));
        assert_eq!(cycle(&mut r, 3), [Client('a'), Host, Client('c')]);
        assert_eq!(r.clients().collect::<Vec<_>>(), ['c', 'd', 'a']);
    }

    #[test]
    fn removing_the_focused_client_returns_focus_to_the_host() {
        let mut r = ring(&['a', 'b']);
        r.advance();
        assert_eq!(r.set_clients(['b']), Some(Host));
        assert_eq!(r.focused(), Host);
        assert_eq!(r.set_clients([]), None);
        assert_eq!(r.advance(), None);
    }

    #[test]
    fn no_clients_stays_on_host() {
        let mut r = ring(&[]);
        assert_eq!(r.advance(), None);
        assert_eq!(r.focused(), Host);
        let mut empty = FocusRing::<char>::new([]);
        assert_eq!(empty.advance(), None);
    }

    #[test]
    fn joining_client_takes_its_configured_place() {
        let mut r = ring(&['a', 'c']);
        assert_eq!(r.advance(), Some(Client('a')));
        assert_eq!(r.set_available('b', true), None, "focus doesn't move");
        assert_eq!(cycle(&mut r, 3), [Client('b'), Client('c'), Host]);
    }

    // NKMS's client-removal edge cases (https://github.com/nicholasyoder/nkms/commit/31a289a).

    #[test]
    fn removing_focused_client_returns_to_host() {
        let mut r = ring(&['a', 'b', 'c']);
        r.advance();
        r.advance(); // b
        assert_eq!(r.set_available('b', false), Some(Host));
        assert_eq!(r.focused(), Host);
        assert_eq!(cycle(&mut r, 3), [Client('a'), Client('c'), Host]);
    }

    #[test]
    fn removing_last_client_while_focused() {
        let mut r = ring(&['a', 'b', 'c']);
        cycle(&mut r, 3); // c
        assert_eq!(r.set_available('c', false), Some(Host));
        assert_eq!(cycle(&mut r, 3), [Client('a'), Client('b'), Host]);
    }

    #[test]
    fn removing_only_client_while_focused() {
        let mut r = ring(&['b']);
        r.advance();
        assert_eq!(r.set_available('b', false), Some(Host));
        assert_eq!(r.advance(), None);
    }

    #[test]
    fn removing_client_before_focused_keeps_focus() {
        let mut r = ring(&['a', 'b', 'c']);
        cycle(&mut r, 2); // b
        assert_eq!(r.set_available('a', false), None);
        assert_eq!(r.focused(), Client('b'));
        assert_eq!(cycle(&mut r, 2), [Client('c'), Host]);
    }

    #[test]
    fn removing_client_after_focused_keeps_focus() {
        let mut r = ring(&['a', 'b', 'c']);
        r.advance(); // a
        assert_eq!(r.set_available('b', false), None);
        assert_eq!(cycle(&mut r, 2), [Client('c'), Host]);
    }

    #[test]
    fn removing_and_rejoining_while_unfocused() {
        let mut r = ring(&['a', 'b']);
        r.set_available('a', false);
        r.set_available('a', true);
        assert_eq!(cycle(&mut r, 3), [Client('a'), Client('b'), Host]);
    }

    #[test]
    fn explicit_focus() {
        let mut r = ring(&['a', 'c']);
        assert_eq!(r.focus(Client('b')), Err(Unavailable));
        assert_eq!(r.focus(Client('z')), Err(Unavailable));
        assert_eq!(r.focus(Client('c')), Ok(Some(Client('c'))));
        assert_eq!(r.focus(Client('c')), Ok(None));
        assert_eq!(r.advance(), Some(Host));
        assert_eq!(r.focus(Host), Ok(None));
    }

    #[test]
    fn unknown_clients_and_duplicates_are_ignored() {
        let mut r = FocusRing::new(['a', 'a']);
        assert_eq!(r.set_available('z', true), None);
        r.set_available('a', true);
        assert_eq!(r.available().collect::<Vec<_>>(), ['a']);
        assert_eq!(cycle(&mut r, 2), [Client('a'), Host]);
    }
}
