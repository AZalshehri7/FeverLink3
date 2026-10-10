//! Thread roots that survive a panic, when the build unwinds.
//!
//! The release profile builds with `panic = "abort"` (the native DLL grows by about 280 KB when it unwinds, see
//! `docs/SECURITY.md`), and then a panic anywhere in the game process ends the game after the panic hook has
//! written the message to the log. Every thread root that matters (the controller, the session threads) still
//! runs its work through [`guarded`], so that switching the profile to `unwind` is the only change needed to turn
//! such a panic into a log line and a stopped session. Tests always unwind, which is how this is tested.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Runs `f`. If it panics, returns the panic's message instead of letting the panic go on up the stack.
///
/// The caller decides what a failed `f` leaves behind: whatever `f` was changing may be half done, so the usual
/// answer is to throw that state away (stop the session, reset the controller) rather than carry on with it.
pub fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| panic_message(payload.as_ref()))
}

/// The text of a panic payload: what `panic!` and `assert!` give (`&str` or `String`), else a placeholder.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_comes_back_and_a_panic_becomes_its_message() {
        assert_eq!(guarded(|| 7), Ok(7));
        assert_eq!(guarded(|| -> u8 { panic!("static text") }), Err("static text".to_owned()));
        let n = 3;
        assert_eq!(guarded(|| -> u8 { panic!("formatted {n}") }), Err("formatted 3".to_owned()));
        assert_eq!(guarded(|| -> u8 { std::panic::panic_any(42u8) }), Err("a panic without a message".to_owned()));
        let v: Vec<u8> = Vec::new();
        let at = v.len() + 1;
        let message = guarded(|| v[at]).unwrap_err();
        assert!(message.contains("index out of bounds"), "{message}");
    }

    #[test]
    fn what_the_closure_changed_before_panicking_is_still_visible_to_the_caller() {
        let mut steps = Vec::new();
        let outcome = guarded(|| {
            steps.push(1);
            steps.push(2);
            panic!("half way");
        });
        assert_eq!(outcome, Err::<(), _>("half way".to_owned()));
        assert_eq!(steps, [1, 2]);
    }
}
