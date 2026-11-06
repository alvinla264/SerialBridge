//! Terminal lifecycle: raw mode and the alternate screen are entered once and
//! restored exactly once, including on errors and panics.

use std::io;

use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};

/// The terminal operations the cleanup guard owns.
pub trait TerminalOps {
    fn enter(&mut self) -> io::Result<()>;
    fn leave(&mut self) -> io::Result<()>;
}

/// Restores the terminal when dropped. `leave` is called once, also after a
/// panic, and its error is ignored because there is nothing safer to do then.
pub struct CleanupGuard<T: TerminalOps> {
    ops: T,
    active: bool,
}

impl<T: TerminalOps> CleanupGuard<T> {
    /// Enter terminal mode. If entering fails part-way, the terminal is
    /// restored immediately and the error returned.
    pub fn new(mut ops: T) -> io::Result<Self> {
        match ops.enter() {
            Ok(()) => Ok(Self { ops, active: true }),
            Err(error) => {
                let _ = ops.leave();
                Err(error)
            }
        }
    }

    /// Restore now. Calling it again (or on drop) does nothing.
    pub fn cleanup(&mut self) {
        if self.active {
            self.active = false;
            let _ = self.ops.leave();
        }
    }
}

impl<T: TerminalOps> Drop for CleanupGuard<T> {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// The real terminal: raw mode, alternate screen, hidden cursor.
pub struct CrosstermOps;

impl TerminalOps for CrosstermOps {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
        Ok(())
    }

    fn leave(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
        disable_raw_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeOps {
        calls: Rc<RefCell<Vec<&'static str>>>,
        enter_fails: bool,
        leave_fails: bool,
    }

    impl FakeOps {
        fn new() -> (Self, Rc<RefCell<Vec<&'static str>>>) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    calls: Rc::clone(&calls),
                    enter_fails: false,
                    leave_fails: false,
                },
                calls,
            )
        }
    }

    impl TerminalOps for FakeOps {
        fn enter(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("enter");
            if self.enter_fails {
                Err(io::Error::other("enter failed"))
            } else {
                Ok(())
            }
        }

        fn leave(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push("leave");
            if self.leave_fails {
                Err(io::Error::other("leave failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn leaving_scope_restores_the_terminal_once() {
        let (ops, calls) = FakeOps::new();
        {
            let _guard = CleanupGuard::new(ops).expect("enter");
            assert_eq!(*calls.borrow(), ["enter"]);
        }
        assert_eq!(*calls.borrow(), ["enter", "leave"]);
    }

    #[test]
    fn explicit_cleanup_is_not_repeated_on_drop() {
        let (ops, calls) = FakeOps::new();
        {
            let mut guard = CleanupGuard::new(ops).expect("enter");
            guard.cleanup();
        }
        assert_eq!(*calls.borrow(), ["enter", "leave"]);
    }

    #[test]
    fn a_panic_still_restores_the_terminal() {
        let (ops, calls) = FakeOps::new();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _guard = CleanupGuard::new(ops).expect("enter");
            panic!("event loop blew up");
        }));
        assert!(panic.is_err());
        assert_eq!(*calls.borrow(), ["enter", "leave"]);
    }

    #[test]
    fn an_enter_error_leaves_after_the_partial_enter() {
        let (mut ops, calls) = FakeOps::new();
        ops.enter_fails = true;
        let error = CleanupGuard::new(ops).err().expect("enter fails");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(*calls.borrow(), ["enter", "leave"]);
    }

    #[test]
    fn a_leave_error_does_not_panic_or_retry() {
        let (mut ops, calls) = FakeOps::new();
        ops.leave_fails = true;
        {
            let _guard = CleanupGuard::new(ops).expect("enter");
        }
        assert_eq!(*calls.borrow(), ["enter", "leave"]);
    }
}
