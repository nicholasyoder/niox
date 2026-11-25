//! Who is using the machine's seat, from logind's runtime files (no
//! D-Bus needed). nioxd gives the audio channel to that user's
//! niox-session.

use std::fs;

/// logind's state file for the default seat.
pub const SEAT0: &str = "/run/systemd/seats/seat0";

/// The uid of the user whose session is active on `seat0`, if any. At a
/// login screen that's the display manager's user; with no seat (or no
/// logind) it's `None`.
pub fn active_uid() -> Option<u32> {
    parse_active_uid(&fs::read_to_string(SEAT0).ok()?)
}

fn parse_active_uid(contents: &str) -> Option<u32> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("ACTIVE_UID="))
        .and_then(|uid| uid.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_logind_seat_files() {
        let seat = "# This is private data. Do not parse.\nIS_SEAT0=1\nCAN_MULTI_SESSION=1\n\
                    CAN_TTY=1\nCAN_GRAPHICAL=1\nACTIVE=2\nACTIVE_UID=1000\nSESSIONS=2 c1\nUIDS=1000 42\n";
        assert_eq!(parse_active_uid(seat), Some(1000));
        assert_eq!(parse_active_uid("IS_SEAT0=1\nSESSIONS=\n"), None);
        assert_eq!(parse_active_uid("ACTIVE_UID=\n"), None);
    }
}
