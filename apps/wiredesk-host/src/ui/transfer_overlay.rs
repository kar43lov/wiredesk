//! Always-on-top mini-overlay that surfaces clipboard transfer progress
//! while a chunk train is in flight. Lives in the bottom-right of the
//! primary monitor as a borderless, popup-style nwg::Window. Auto-hides
//! when both directions are idle.
//!
//! Threading: the four atomics are written by the session thread (sole
//! writer) and read by an `nwg::AnimationTimer` ticking on the UI thread.
//! No locks anywhere in the hot path.
//!
//! Pure helper [`render_overlay_text`] is unit-tested cross-platform; the
//! nwg wiring (window build + timer wiring) is `#[cfg(windows)]`.

#![cfg_attr(not(windows), allow(dead_code))]

// `ProgressCounters` is consumed by the `windows_impl` submodule (the
// nwg-driven overlay) and by the cross-platform unit tests below. On
// non-Windows non-test builds neither consumer compiles, so the import
// would warn — silence the warning rather than gate it three different
// ways.
#[cfg_attr(not(any(windows, test)), allow(unused_imports))]
use crate::clipboard::ProgressCounters;

/// Render the overlay's single-line text, OR `None` if nothing is in flight.
///
/// Convention: `total == 0` means idle. When both directions have non-zero
/// totals we concatenate so the user sees both transfers (the common case
/// is that only one direction is active at a time).
///
/// Bytes-format mirrors the Mac status-line: switch to KB once the total
/// crosses 1 KiB so a tiny text snippet doesn't show as "0/0 KB".
pub fn render_overlay_text(
    out_progress: u64,
    out_total: u64,
    in_progress: u64,
    in_total: u64,
) -> Option<String> {
    let outgoing = format_one("Sending", out_progress, out_total);
    let incoming = format_one("Receiving", in_progress, in_total);
    match (outgoing, incoming) {
        (None, None) => None,
        (Some(s), None) => Some(format!("\u{2191} {s}")),
        (None, Some(s)) => Some(format!("\u{2193} {s}")),
        (Some(out), Some(inc)) => Some(format!("\u{2191} {out}  /  \u{2193} {inc}")),
    }
}

fn format_one(action: &str, current: u64, total: u64) -> Option<String> {
    if total == 0 {
        return None;
    }
    let cur = current.min(total);
    let pct = (cur * 100) / total;
    if total < 1024 {
        Some(format!("{action} {cur}/{total} B ({pct}%)"))
    } else {
        let cur_kb = cur / 1024;
        let tot_kb = total / 1024;
        Some(format!("{action} {cur_kb}/{tot_kb} KB ({pct}%)"))
    }
}

/// Decide whether we are "completed" — both totals are non-zero AND
/// every active direction has reached its total. Used to drive the 1 s
/// post-completion latch before hiding.
pub fn transfer_completed(
    out_progress: u64,
    out_total: u64,
    in_progress: u64,
    in_total: u64,
) -> bool {
    let any_active = out_total > 0 || in_total > 0;
    if !any_active {
        return false;
    }
    let out_done = out_total == 0 || out_progress >= out_total;
    let in_done = in_total == 0 || in_progress >= in_total;
    out_done && in_done
}

#[cfg(windows)]
pub use windows_impl::TransferOverlay;

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use native_windows_gui as nwg;

    /// Hold-time at 100% before the overlay hides — gives the user a
    /// moment to register that the transfer finished.
    const POST_COMPLETE_HOLD: Duration = Duration::from_millis(1000);

    /// Polling cadence — 250 ms is fast enough to feel live but cheap
    /// enough that the AnimationTimer-driven repaint isn't noticeable.
    const POLL_INTERVAL: Duration = Duration::from_millis(250);

    /// Overlay size (logical pixels). Chosen to fit a single line of the
    /// "Sending image — N/M KB (P%)" string with comfortable padding.
    const OVERLAY_WIDTH: i32 = 290;
    const OVERLAY_HEIGHT: i32 = 32;

    /// Distance of the overlay's right/bottom edge from the work-area
    /// corner. The work area excludes the taskbar so the overlay never
    /// lands on top of it.
    const RIGHT_MARGIN: i32 = 16;
    const BOTTOM_MARGIN: i32 = 50;

    pub struct TransferOverlay {
        pub window: nwg::Window,
        pub label: nwg::Label,
        pub timer: nwg::AnimationTimer,

        counters: ProgressCounters,
        /// First instant at which we observed `transfer_completed == true`
        /// for the current cycle. `Some` while the 100% latch is running;
        /// reset to `None` once the overlay hides or a new transfer starts.
        completed_at: RefCell<Option<Instant>>,
    }

    impl TransferOverlay {
        pub fn build(counters: ProgressCounters) -> Result<Rc<RefCell<Self>>, nwg::NwgError> {
            let me = Rc::new(RefCell::new(Self {
                window: Default::default(),
                label: Default::default(),
                timer: Default::default(),
                counters,
                completed_at: RefCell::new(None),
            }));
            {
                let mut s = me.borrow_mut();
                // Reborrow through `&mut *` so the compiler can split
                // disjoint-field borrows on `window`, `label`, `timer`.
                // Going through `RefMut` directly defeats the splitting
                // because `Deref::deref_mut` returns a single `&mut Self`.
                let s_ref: &mut Self = &mut s;
                let (x, y) = bottom_right_position(OVERLAY_WIDTH, OVERLAY_HEIGHT);
                build_controls(&mut s_ref.window, &mut s_ref.label, &mut s_ref.timer, (x, y))?;
            }
            Ok(me)
        }

        /// Read counters once, update label visibility/text. Called from
        /// the AnimationTimer's `OnTimerTick` event handler.
        pub fn tick(&self) {
            let out_p = self.counters.outgoing_progress.load(Ordering::Relaxed);
            let out_t = self.counters.outgoing_total.load(Ordering::Relaxed);
            let in_p = self.counters.incoming_progress.load(Ordering::Relaxed);
            let in_t = self.counters.incoming_total.load(Ordering::Relaxed);

            let text = render_overlay_text(out_p, out_t, in_p, in_t);

            // 100% latch: once complete, hold the banner visible for ~1 s
            // before hiding. Fresh activity (counters bump) clears the
            // latch.
            let completed = transfer_completed(out_p, out_t, in_p, in_t);
            let mut completed_at = self.completed_at.borrow_mut();
            if completed {
                if completed_at.is_none() {
                    *completed_at = Some(Instant::now());
                }
            } else {
                *completed_at = None;
            }

            match text {
                Some(s) => {
                    // If we're inside the post-completion hold, keep
                    // showing the 100% string. After the hold expires,
                    // hide regardless.
                    if let Some(at) = *completed_at {
                        if at.elapsed() >= POST_COMPLETE_HOLD {
                            self.window.set_visible(false);
                            self.label.set_text("");
                            return;
                        }
                    }
                    self.label.set_text(&s);
                    if !self.window.visible() {
                        self.window.set_visible(true);
                    }
                }
                None => {
                    self.window.set_visible(false);
                    self.label.set_text("");
                }
            }
        }

        pub fn timer_handle(&self) -> nwg::ControlHandle {
            self.timer.handle
        }
    }

    fn build_controls(
        window: &mut nwg::Window,
        label: &mut nwg::Label,
        timer: &mut nwg::AnimationTimer,
        (x, y): (i32, i32),
    ) -> Result<(), nwg::NwgError> {
        // WS_EX_TOOLWINDOW (excluded from Alt-Tab) is set via ex_flags.
        const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

        // Use the default `WindowFlags::WINDOW` (overlapped). nwg 1.0.13's
        // `WindowFlags::POPUP` produces an HWND that AnimationTimer/Label
        // refuse to bind to with "Cannot bind control with an handle of
        // type" — see commit message. We strip the title bar / system
        // menu post-build via SetWindowLongPtrW instead, which gives the
        // borderless look without changing the window class.
        nwg::Window::builder()
            .size((OVERLAY_WIDTH, OVERLAY_HEIGHT))
            .position((x, y))
            .title("WireDesk Transfer")
            .flags(nwg::WindowFlags::WINDOW)
            .ex_flags(WS_EX_TOOLWINDOW)
            .topmost(true)
            .build(window)?;

        // TODO: strip WS_CAPTION/WS_THICKFRAME via SetWindowLongPtrW so
        // the overlay renders as a flat rectangle instead of a normal
        // window with a title bar. First-iteration goal is just "host
        // doesn't panic on startup"; cosmetic strip is a follow-up.

        nwg::Label::builder()
            .text("")
            .h_align(nwg::HTextAlign::Center)
            .parent(&*window)
            .build(label)?;

        // Layout: single label filling the whole window.
        let mut layout = nwg::GridLayout::default();
        nwg::GridLayout::builder()
            .parent(&*window)
            .max_column(Some(1))
            .spacing(0)
            .margin([6, 6, 6, 6])
            .child(0, 0, &*label)
            .build(&mut layout)?;
        // Layout is owned by the closure scope — nwg keeps it alive via
        // the parent window. Safe to drop here.
        std::mem::forget(layout);

        // AnimationTimer without `.parent()` — the default message-only
        // host window receives the timer ticks. Binding it to a real
        // top-level window paniced in nwg 1.0.13 ("Cannot bind control
        // with an handle of type"); the timer doesn't need the parent
        // anyway, only the event handler binds tick → tick().
        nwg::AnimationTimer::builder()
            .interval(POLL_INTERVAL)
            .active(true)
            .build(timer)?;

        // Start hidden — `tick()` flips visibility based on counters.
        window.set_visible(false);

        Ok(())
    }


    /// Compute the overlay's top-left position so its right/bottom edge
    /// lands `RIGHT_MARGIN` / `BOTTOM_MARGIN` away from the primary
    /// monitor's work-area corner. Falls back to `(0, 0)` if the Win32
    /// query fails (extremely rare; logged as warning).
    fn bottom_right_position(w: i32, h: i32) -> (i32, i32) {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETWORKAREA, SystemParametersInfoW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };

        let mut rect = RECT::default();
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut rect as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        if ok.is_err() {
            log::warn!("transfer_overlay: SPI_GETWORKAREA failed; placing at (0,0)");
            return (0, 0);
        }
        let x = rect.right - w - RIGHT_MARGIN;
        let y = rect.bottom - h - BOTTOM_MARGIN;
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_overlay_text_idle() {
        assert_eq!(render_overlay_text(0, 0, 0, 0), None);
    }

    #[test]
    fn render_overlay_text_sending_only() {
        let s = render_overlay_text(340 * 1024, 780 * 1024, 0, 0).expect("active");
        assert!(s.contains("\u{2191}"), "should have up-arrow: {s}");
        assert!(!s.contains("\u{2193}"), "no down-arrow when idle inbound: {s}");
        assert!(s.contains("Sending"));
        assert!(s.contains("340/780 KB"));
        assert!(s.contains("(43%)"));
    }

    #[test]
    fn render_overlay_text_receiving_only() {
        let s = render_overlay_text(0, 0, 256, 1024).expect("active");
        assert!(s.contains("\u{2193}"), "should have down-arrow: {s}");
        assert!(!s.contains("\u{2191}"), "no up-arrow when idle outbound: {s}");
        assert!(s.contains("Receiving"));
        assert!(s.contains("0/1 KB") || s.contains("0/1 KB"));
        assert!(s.contains("(25%)"));
    }

    #[test]
    fn render_overlay_text_both_directions() {
        let s = render_overlay_text(100, 200, 300, 400).expect("active");
        assert!(s.contains("\u{2191}"));
        assert!(s.contains("\u{2193}"));
        assert!(s.contains("Sending"));
        assert!(s.contains("Receiving"));
    }

    #[test]
    fn render_overlay_text_sub_kb_uses_bytes() {
        let s = render_overlay_text(25, 50, 0, 0).expect("active");
        assert!(s.contains("25/50 B"), "sub-KB transfer must use bytes: {s}");
    }

    #[test]
    fn render_overlay_text_clamps_overshoot() {
        // Brief race: writer bumped progress past total before total
        // was rolled into the next transfer. The percent must clamp.
        let s = render_overlay_text(2048, 1024, 0, 0).expect("active");
        assert!(s.contains("(100%)"), "overshoot must clamp to 100%: {s}");
    }

    #[test]
    fn transfer_completed_idle_returns_false() {
        assert!(!transfer_completed(0, 0, 0, 0));
    }

    #[test]
    fn transfer_completed_in_flight_returns_false() {
        assert!(!transfer_completed(50, 100, 0, 0));
    }

    #[test]
    fn transfer_completed_outgoing_done_returns_true() {
        assert!(transfer_completed(100, 100, 0, 0));
    }

    #[test]
    fn transfer_completed_overshoot_counts_as_done() {
        assert!(transfer_completed(120, 100, 0, 0));
    }

    #[test]
    fn transfer_completed_one_active_one_idle_uses_active_only() {
        // Outgoing finished, no incoming activity → completed.
        assert!(transfer_completed(100, 100, 0, 0));
        // Outgoing finished but incoming still in flight → NOT completed.
        assert!(!transfer_completed(100, 100, 50, 100));
    }
}

#[cfg(test)]
mod observer_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn render_uses_atomics_via_counters() {
        // End-to-end: write to atomics, observer reads, render produces
        // a non-empty string. Verifies the counters→render pipeline.
        let c = ProgressCounters::default();
        c.outgoing_total.store(2048, Ordering::Relaxed);
        c.outgoing_progress.store(1024, Ordering::Relaxed);
        let s = render_overlay_text(
            c.outgoing_progress.load(Ordering::Relaxed),
            c.outgoing_total.load(Ordering::Relaxed),
            c.incoming_progress.load(Ordering::Relaxed),
            c.incoming_total.load(Ordering::Relaxed),
        )
        .expect("active");
        assert!(s.contains("\u{2191}"));
        assert!(s.contains("1/2 KB"));
        assert!(s.contains("(50%)"));
    }
}
