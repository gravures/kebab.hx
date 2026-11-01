use abi_stable::{
    std_types::{
        RBoxError,
        RResult::{self},
        RString,
    },
    RMut,
};
use config::{ColorAttribute, SrgbaTuple, TermConfig};
use portable_pty::{
    Child, CommandBuilder, MasterPty, NativePtySystem, PtyPair, PtySize, PtySystem,
};
use std::{
    any::Any,
    io::{Read, Write},
    sync::{
        mpsc::{channel, Sender},
        Arc,
    },
};
use steel::{
    declare_module,
    rvals::Custom,
    steel_vm::ffi::{
        as_underlying_ffi_type, CustomRef, FFIArg, FFIModule, FFIValue, FfiFuture, FfiFutureExt,
        IntoFFIVal, RegisterFFIFn, VectorRef,
    },
};
use wezterm_term::{Cell, Line, Terminal};

use tokio::sync::mpsc;

#[derive(Clone)]
pub(crate) struct WriterWrapper {
    writer: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,
}

// Note: This is no bueno, but we'll need this for now
// until we figure out how to relax some of the constraints. I'm guessing
// that the FFI layer probably just needs to use Mutex instead of RwLock
// to avoid the Sync problem.
unsafe impl Send for PtyProcess {}
unsafe impl Sync for PtyProcess {}

unsafe impl Send for VirtualTerminal {}
unsafe impl Sync for VirtualTerminal {}

impl WriterWrapper {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Arc::new(parking_lot::Mutex::new(writer)),
        }
    }
}

impl std::io::Write for WriterWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.lock().flush()
    }
}

struct PtyProcess {
    cancellation_token_sender: Sender<()>,
    command_sender: Sender<u8>,
    async_receiver: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync + 'static>,
    writer: WriterWrapper,
}

use futures::{future::Either, lock::Mutex, FutureExt};

// #[derive(Clone)]
// struct TermAction(Action);

// impl Custom for TermAction {}

#[derive(Debug)]
struct PtyProcessError(anyhow::Error);
impl std::error::Error for PtyProcessError {}

impl std::fmt::Display for PtyProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#?}", self.0)
    }
}

#[derive(Debug)]
struct PanicError {
    name: &'static str,
    payload: String,
    backtrace: String,
}

impl PanicError {
    fn new(name: &'static str, payload: Box<dyn Any + Send>) -> Self {
        let payload = if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else {
            "unknown panic payload".to_string()
        };

        Self {
            name,
            payload,
            backtrace: std::backtrace::Backtrace::force_capture().to_string(),
        }
    }
}

impl std::fmt::Display for PanicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "panic in {}: {}\n\nRust backtrace:\n{}",
            self.name, self.payload, self.backtrace
        )
    }
}

impl std::error::Error for PanicError {}

fn guard_ffi<T: IntoFFIVal>(
    name: &'static str,
    f: impl FnOnce() -> T,
) -> RResult<FFIValue, RBoxError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f().into_ffi_val())) {
        Ok(result) => result,
        Err(payload) => RResult::RErr(RBoxError::new(PanicError::new(name, payload))),
    }
}

impl PtyProcess {
    pub fn kill(&mut self) {
        self.child.kill().ok();
        self.cancellation_token_sender.send(()).ok();
    }

    // TODO: Replace this with a proper result rather than a bool
    pub fn send_command_char(&mut self, command: char) -> bool {
        self.command_sender
            .send(command as u8)
            .map(|_| true)
            .unwrap_or(false)
    }

    // TODO: Replace this with a proper result rather than a bool
    pub fn send_command(&mut self, command: &str) -> bool {
        for byte in command.as_bytes() {
            if self.command_sender.send(*byte).is_err() {
                return false;
            }
        }

        true
    }

    // TODO: rows + cols should be u16,
    // and those bounds checks should be implemented on
    // the conversion
    pub fn resize(&mut self, rows: usize, cols: usize) -> RResult<FFIValue, RBoxError> {
        match self.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(_) => RResult::ROk(FFIValue::Void),
            Err(e) => RResult::RErr(RBoxError::new(PtyProcessError(e))),
        }
    }

    // Attempt to move the bytes without cloning the heap allocation underneath?
    pub fn async_try_read_line(&mut self) -> FfiFuture<RResult<FFIValue, RBoxError>> {
        let ar = Arc::clone(&self.async_receiver);

        std::panic::AssertUnwindSafe(async move {
            let mut guard = ar.lock().await;

            let mut buffer = String::new();

            // Optimistically read as much as we can into this buffer.
            // Yield back once we have nothing else.
            while let Ok(v) = guard.try_recv() {
                buffer.push_str(&v);
            }

            let next = guard.recv();
            let timeout = futures_time::task::sleep(futures_time::time::Duration::from_millis(12));

            futures::pin_mut!(next);

            match futures::future::select(next, timeout).await {
                Either::Left((Some(message), _)) => {
                    buffer.push_str(&message);
                    RResult::ROk(FFIValue::StringV(buffer.into()))
                }
                Either::Left((None, _)) => {
                    if buffer.is_empty() {
                        RResult::ROk(FFIValue::BoolV(false))
                    } else {
                        RResult::ROk(FFIValue::StringV(buffer.into()))
                    }
                }
                Either::Right((_, fut)) => {
                    if buffer.is_empty() {
                        fut.map(|x| {
                            if let Some(message) = x {
                                buffer.push_str(&message);

                                RResult::ROk(FFIValue::StringV(buffer.into()))
                            } else {
                                RResult::ROk(FFIValue::BoolV(false))
                            }
                        })
                        .await
                    } else {
                        RResult::ROk(FFIValue::StringV(buffer.into()))
                    }
                }
            }
        })
        .catch_unwind()
        .map(|result| match result {
            Ok(value) => value,
            Err(payload) => RResult::ROk(FFIValue::Vector(
                vec![
                    FFIValue::StringV(RString::from("panic")),
                    FFIValue::StringV(RString::from(
                        PanicError::new("async-try-read-line", payload).to_string(),
                    )),
                ]
                .into(),
            )),
        })
        .into_ffi()
    }
}

impl Custom for PtyProcess {}

impl Drop for PtyProcess {
    fn drop(self: &mut PtyProcess) {
        self.kill();
    }
}

declare_module!(create_module);

fn create_module() -> FFIModule {
    let mut module = FFIModule::new("steel/pty-process");

    module
        .register_fn("create-native-pty-system!", |command: String| {
            guard_ffi("create-native-pty-system!", move || {
                create_native_pty_system(command)
            })
        })
        .register_fn(
            "create-native-pty-system-with-cwd!",
            |command: String, cwd: String| {
                guard_ffi(
                    "create-native-pty-system-with-cwd!",
                    move || create_native_pty_system_with_cwd(command, cwd),
                )
            },
        )
        .register_fn(
            "create-native-pty-system-with-args-and-cwd!",
            |command: String, args: Vec<String>, cwd: String| {
                guard_ffi(
                    "create-native-pty-system-with-args-and-cwd!",
                    move || create_native_pty_system_with_args_and_cwd(command, args, cwd),
                )
            },
        )
        .register_fn("kill-pty-process!", |pty: &mut PtyProcess| {
            guard_ffi("kill-pty-process!", move || PtyProcess::kill(pty))
        })
        .register_fn(
            "pty-process-send-command",
            |pty: &mut PtyProcess, command: &str| {
                guard_ffi(
                    "pty-process-send-command",
                    move || PtyProcess::send_command(pty, command),
                )
            },
        )
        .register_fn(
            "pty-process-send-command-char",
            |pty: &mut PtyProcess, command: char| {
                guard_ffi(
                    "pty-process-send-command-char",
                    move || PtyProcess::send_command_char(pty, command),
                )
            },
        )
        .register_fn("async-try-read-line", |pty: &mut PtyProcess| {
            guard_ffi("async-try-read-line", move || {
                PtyProcess::async_try_read_line(pty)
            })
        })
        .register_fn(
            "pty-resize!",
            |pty: &mut PtyProcess, rows: usize, cols: usize| {
                guard_ffi("pty-resize!", move || PtyProcess::resize(pty, rows, cols))
            },
        )
        .register_fn("virtual-terminal", |pty: &mut PtyProcess| {
            guard_ffi("virtual-terminal", move || VirtualTerminal {
                terminal: Terminal::new(
                    wezterm_term::TerminalSize::default(),
                    Arc::new(TermConfig::new()),
                    "bash",
                    "0.1.0",
                    Box::new(pty.writer.clone()),
                ),
                screen_iterator: ScreenCellIterator { x: 0, y: 0 },
                last_cell: None,
                last_reverse: false,
                run_x: 0,
                run_y: 0,
                scroll_up_modifier: 0,
            })
        })
        // Raw virtual terminal!
        .register_fn("raw-virtual-terminal", || {
            guard_ffi("raw-virtual-terminal", move || VirtualTerminal {
                terminal: Terminal::new(
                    wezterm_term::TerminalSize::default(),
                    Arc::new(TermConfig::new()),
                    "bash",
                    "0.1.0",
                    Box::new(std::io::empty()),
                ),
                screen_iterator: ScreenCellIterator { x: 0, y: 0 },
                last_cell: None,
                last_reverse: false,
                run_x: 0,
                run_y: 0,
                scroll_up_modifier: 0,
            })
        })
        .register_fn("vte/advance-bytes", |term: &mut VirtualTerminal, bytes: &str| {
            guard_ffi("vte/advance-bytes", move || {
                VirtualTerminal::advance_bytes(term, bytes)
            })
        })
        // Advancing with immediate action
        .register_fn(
            "vte/advance-bytes-char",
            |term: &mut VirtualTerminal, bytes: char| {
                guard_ffi("vte/advance-bytes-char", move || {
                    VirtualTerminal::advance_bytes_char(term, bytes)
                })
            },
        )
        .register_fn(
            "vte/advance-bytes-with-carriage-return",
            |term: &mut VirtualTerminal, bytes: &str| {
                guard_ffi("vte/advance-bytes-with-carriage-return", move || {
                    VirtualTerminal::advance_bytes_with_carriage_return(term, bytes)
                })
            },
        )
        .register_fn(
            "vte/resize",
            |term: &mut VirtualTerminal, rows: usize, cols: usize| {
                guard_ffi("vte/resize", move || {
                    VirtualTerminal::resize(term, rows, cols)
                })
            },
        )
        .register_fn("vte/lines", |term: &mut VirtualTerminal| {
            guard_ffi("vte/lines", move || VirtualTerminal::lines(term))
        })
        .register_fn("vte/line->string", |line: &TermLine| {
            guard_ffi("vte/line->string", move || TermLine::as_str(line))
        })
        .register_fn("vte/cursor", |term: &VirtualTerminal| {
            guard_ffi("vte/cursor", move || VirtualTerminal::cursor(term))
        })
        .register_fn("vte/cursor-visible?", |term: &VirtualTerminal| {
            guard_ffi("vte/cursor-visible?", move || {
                VirtualTerminal::cursor_visible(term)
            })
        })
        .register_fn("vte/cursor-x", |term: &VirtualTerminal| {
            guard_ffi("vte/cursor-x", move || VirtualTerminal::cursor_x(term))
        })
        .register_fn("vte/cursor-y", |term: &VirtualTerminal| {
            guard_ffi("vte/cursor-y", move || VirtualTerminal::cursor_y(term))
        })
        .register_fn("vte/line->cells", |line: &mut TermLine| {
            guard_ffi("vte/line->cells", move || {
                line.line
                    .cells_mut()
                    .iter()
                    .cloned()
                    .map(|cell| TermCell { cell }.into_ffi_val().unwrap())
                    .collect::<Vec<FFIValue>>()
            })
        })
        .register_fn("vte/cell->fg", |cell: &TermCell| {
            guard_ffi("vte/cell->fg", move || {
                TermColorAttribute(cell.cell.attrs().foreground())
            })
        })
        .register_fn("vte/cell->bg", |cell: &TermCell| {
            guard_ffi("vte/cell->bg", move || {
                TermColorAttribute(cell.cell.attrs().background())
            })
        })
        // Get the color attribute, map it to the one that helix uses
        // TODO: Re-use the memory - we should pass in an FFI Vector, and then just reuse it over and over.
        .register_fn(
            "term/color-attribute",
            |attribute: &TermColorAttribute| {
                guard_ffi("term/color-attribute", move || match attribute.0 {
                    ColorAttribute::TrueColorWithPaletteFallback(SrgbaTuple(r, g, b, a), _) => vec![
                        ((r * 255.0).round() as isize).into_ffi_val().unwrap(),
                        ((g * 255.0).round() as isize).into_ffi_val().unwrap(),
                        ((b * 255.0).round() as isize).into_ffi_val().unwrap(),
                        ((a * 255.0).round() as isize).into_ffi_val().unwrap(),
                    ]
                    .into_ffi_val(),
                    ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(r, g, b, a)) => vec![
                        ((r * 255.0).round() as isize).into_ffi_val().unwrap(),
                        ((g * 255.0).round() as isize).into_ffi_val().unwrap(),
                        ((b * 255.0).round() as isize).into_ffi_val().unwrap(),
                        ((a * 255.0).round() as isize).into_ffi_val().unwrap(),
                    ]
                    .into_ffi_val(),
                    ColorAttribute::PaletteIndex(index) => (index as usize).into_ffi_val(),
                    ColorAttribute::Default => false.into_ffi_val(),
                })
            },
        )
        .register_fn(
            "term/color-attribute-set!",
            |attribute: &TermColorAttribute, shared_vec: FFIArg| {
                guard_ffi("term/color-attribute-set!", move || {
                    if let FFIArg::VectorRef(VectorRef { mut vec, .. }) = shared_vec {
                        match attribute.0 {
                            ColorAttribute::TrueColorWithPaletteFallback(SrgbaTuple(r, g, b, a), _) => {
                                // Update in place.
                                vec[0] = FFIValue::IntV((r * 255.0).round() as isize);
                                vec[1] = FFIValue::IntV((g * 255.0).round() as isize);
                                vec[2] = FFIValue::IntV((b * 255.0).round() as isize);
                                vec[3] = FFIValue::IntV((a * 255.0).round() as isize);

                                true.into_ffi_val()
                            }
                            ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(r, g, b, a)) => {
                                vec[0] = FFIValue::IntV((r * 255.0).round() as isize);
                                vec[1] = FFIValue::IntV((g * 255.0).round() as isize);
                                vec[2] = FFIValue::IntV((b * 255.0).round() as isize);
                                vec[3] = FFIValue::IntV((a * 255.0).round() as isize);

                                true.into_ffi_val()
                            }
                            ColorAttribute::PaletteIndex(index) => (index as usize).into_ffi_val(),
                            ColorAttribute::Default => false.into_ffi_val(),
                        }
                    } else {
                        false.into_ffi_val()
                    }
                })
            },
        )
        .register_fn("vte/cell-width", |cell: &TermCell| {
            guard_ffi("vte/cell-width", move || cell.cell.width())
        })
        .register_fn("vte/cell-string", |cell: &TermCell| {
            guard_ffi("vte/cell-string", move || cell.cell.str().to_string())
        })
        .register_fn("vte/reset-iterator!", |term: &mut VirtualTerminal| {
            guard_ffi("vte/reset-iterator!", move || {
                term.screen_iterator.x = 0;
                term.screen_iterator.y = term.scroll_up_modifier;
                term.last_cell = None;
            })
        })
        // This should move forward until we actually have something meaningful,
        // rather than arbitrarily stepping forward one step.
        .register_fn("vte/advance-iterator!", |term: &mut VirtualTerminal| {
            guard_ffi("vte/advance-iterator!", move || {
                let size = term.terminal.get_size();

                let (rows, cols) = (size.rows as i64 + term.scroll_up_modifier, size.cols);

                if term.screen_iterator.x < cols && term.screen_iterator.y < rows {
                    term.last_cell = term
                        .terminal
                        .screen_mut()
                        .get_cell_scrollback(term.screen_iterator.x, term.screen_iterator.y as _)
                        .cloned();

                    term.screen_iterator.x += 1;
                    return true;
                }

                if term.screen_iterator.x >= cols && term.screen_iterator.y < rows {
                    term.last_cell = term
                        .terminal
                        .screen_mut()
                        .get_cell_scrollback(term.screen_iterator.x, term.screen_iterator.y as _)
                        .cloned();

                    term.screen_iterator.x = 0;
                    term.screen_iterator.y += 1;

                    return true;
                }

                false
            })
        })
        // Advance until there is a string?
        .register_fn(
            "vte/advance-iterator-until-string!",
            |term: &mut VirtualTerminal| {
                guard_ffi("vte/advance-iterator-until-string!", move || {
                    let size = term.terminal.get_size();

                    let (rows, cols) = (size.rows as i64 + term.scroll_up_modifier, size.cols);

                    loop {
                        if term.screen_iterator.x < cols && term.screen_iterator.y < rows {
                            let last_cell = term.terminal.screen_mut().get_cell_scrollback(
                                term.screen_iterator.x,
                                term.screen_iterator.y as _,
                            );

                            term.screen_iterator.x += 1;

                            if last_cell.is_some() {
                                term.last_cell = last_cell.cloned();
                                return true;
                            } else {
                                continue;
                            }
                        }

                        if term.screen_iterator.x >= cols && term.screen_iterator.y < rows {
                            let last_cell = term.terminal.screen_mut().get_cell_scrollback(
                                term.screen_iterator.x,
                                term.screen_iterator.y as _,
                            );

                            term.screen_iterator.x = 0;
                            term.screen_iterator.y += 1;

                            if last_cell.is_some() {
                                term.last_cell = last_cell.cloned();
                                return true;
                            } else {
                                continue;
                            }
                        } else {
                            return false;
                        }
                    }
                })
            },
        )
        .register_fn(
            "vte/advance-iterator-and-update-cells!",
            |term: &mut VirtualTerminal, mut_str: RMut<'_, RString>, bg: FFIArg, fg: FFIArg| {
                guard_ffi(
                    "vte/advance-iterator-and-update-cells!",
                    move || {
                        let size = term.terminal.get_size();

                        let (rows, cols) = (size.rows as i64 + term.scroll_up_modifier, size.cols);

                        loop {
                            if term.screen_iterator.x < cols && term.screen_iterator.y < rows {
                                let last_cell = term.terminal.screen_mut().get_cell_scrollback(
                                    term.screen_iterator.x,
                                    term.screen_iterator.y as _,
                                );

                                term.screen_iterator.x += 1;

                                if let Some(cell) = last_cell {
                                    // term.last_cell = last_cell.cloned();
                                    if is_invisible(cell) {
                                        continue;
                                    }
                                    term.last_reverse = cell.attrs().reverse();
                                    update_cell(cell, mut_str, bg, fg);

                                    return true;
                                } else {
                                    continue;
                                }
                            }

                            if term.screen_iterator.x >= cols && term.screen_iterator.y < rows {
                                let last_cell = term.terminal.screen_mut().get_cell_scrollback(
                                    term.screen_iterator.x,
                                    term.screen_iterator.y as _,
                                );

                                term.screen_iterator.x = 0;
                                term.screen_iterator.y += 1;

                                if let Some(cell) = last_cell {
                                    // term.last_cell = last_cell.cloned();
                                    if is_invisible(cell) {
                                        continue;
                                    }
                                    term.last_reverse = cell.attrs().reverse();
                                    update_cell(cell, mut_str, bg, fg);

                                    return true;
                                } else {
                                    continue;
                                }
                            } else {
                                return false;
                            }
                        }
                    },
                )
            },
        )
        // Batched sibling of vte/advance-iterator-and-update-cells!: instead of one
        // visible cell per call, consume the maximal horizontal run of visible cells
        // sharing (fg, bg, reverse) and write them into mut_str as one string. The
        // style attrs are written once per run; run start coords land in run_x/run_y
        // (read them via vte/iter-run-x / vte/iter-run-y AFTER a #t return). Invisible
        // cells and wide-glyph continuation cells (both is_invisible) break the run;
        // a wide glyph itself stays in the run and keeps its full multi-column string,
        // so helix's per-char width advance keeps positions aligned.
        .register_fn(
            "vte/advance-run!",
            |term: &mut VirtualTerminal, mut mut_str: RMut<'_, RString>, bg: FFIArg, fg: FFIArg| {
                guard_ffi("vte/advance-run!", move || {
                    let size = term.terminal.get_size();
                    let (rows, cols) = (size.rows as i64 + term.scroll_up_modifier, size.cols);

                    // Find the next visible cell, same walk as the per-cell variant.
                    let first = loop {
                        if term.screen_iterator.y >= rows {
                            return false;
                        }
                        if term.screen_iterator.x >= cols {
                            term.screen_iterator.x = 0;
                            term.screen_iterator.y += 1;
                            continue;
                        }
                        let cell = term.terminal.screen_mut().get_cell_scrollback(
                            term.screen_iterator.x,
                            term.screen_iterator.y as _,
                        );
                        term.screen_iterator.x += 1;
                        match cell {
                            Some(cell) if !is_invisible(cell) => break cell.clone(),
                            _ => continue,
                        }
                    };

                    term.run_x = term.screen_iterator.x - 1;
                    term.run_y = term.screen_iterator.y - term.scroll_up_modifier;

                    let attrs = first.attrs().to_owned();
                    term.last_reverse = attrs.reverse();

                    let mut_str = mut_str.get_mut();
                    mut_str.clear();
                    mut_str.push_str(first.str());

                    // Extend while cells stay visible and share (fg, bg, reverse).
                    while term.screen_iterator.x < cols {
                        let cell = term.terminal.screen_mut().get_cell_scrollback(
                            term.screen_iterator.x,
                            term.screen_iterator.y as _,
                        );
                        match cell {
                            Some(cell)
                                if !is_invisible(cell)
                                    && cell.attrs().foreground() == attrs.foreground()
                                    && cell.attrs().background() == attrs.background()
                                    && cell.attrs().reverse() == attrs.reverse() =>
                            {
                                mut_str.push_str(cell.str());
                                term.screen_iterator.x += 1;
                            }
                            _ => break,
                        }
                    }

                    if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = bg {
                        if let Some(attr) =
                            as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                        {
                            attr.0 = attrs.background();
                        }
                    }
                    if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = fg {
                        if let Some(attr) =
                            as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                        {
                            attr.0 = attrs.foreground();
                        }
                    }
                    true
                })
            },
        )
        .register_fn("vte/iter-run-x", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-run-x", move || term.run_x as isize)
        })
        .register_fn("vte/iter-run-y", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-run-y", move || term.run_y as isize)
        })
        .register_fn("vte/iter-x", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-x", move || term.screen_iterator.x)
        })
        .register_fn("vte/iter-y", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-y", move || {
                (term.screen_iterator.y - term.scroll_up_modifier) as isize
            })
        })
        // TODO: Add function to mutate in place
        .register_fn("vte/iter-cell-fg", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-cell-fg", move || {
                if let Some(cell) = &term.last_cell {
                    TermColorAttribute(cell.attrs().to_owned().foreground()).into_ffi_val()
                } else {
                    false.into_ffi_val()
                }
            })
        })
        // TODO: Add function to mutate in place
        .register_fn("vte/iter-cell-bg", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-cell-bg", move || {
                if let Some(cell) = &term.last_cell {
                    TermColorAttribute(cell.attrs().to_owned().background()).into_ffi_val()
                } else {
                    false.into_ffi_val()
                }
            })
        })
        .register_fn("vte/iter-cell-reverse?", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-cell-reverse?", move || term.last_reverse)
        })
        .register_fn("vte/empty-cell", || {
            guard_ffi("vte/empty-cell", move || {
                TermColorAttribute(ColorAttribute::default())
            })
        })
        // Steel has no reader for an ffi MutableString, so a caller that wants
        // the text of a painted run has to go through here.
        .register_fn("mutable-string-to-string", |s: RMut<'_, RString>| {
            guard_ffi("mutable-string-to-string", move || s.get().as_str().to_string())
        })
        .register_fn(
            "vte/iter-cell-bg-fg-set-attr!",
            |term: &VirtualTerminal, bg: FFIArg, fg: FFIArg| {
                guard_ffi("vte/iter-cell-bg-fg-set-attr!", move || {
                    if let Some(cell) = &term.last_cell {
                        if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = bg {
                            if let Some(attr) =
                                as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                            {
                                attr.0 = cell.attrs().background();
                            }
                        }

                        if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = fg {
                            if let Some(attr) =
                                as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                            {
                                attr.0 = cell.attrs().foreground();

                                return true.into_ffi_val();
                            } else {
                                return false.into_ffi_val();
                            }
                        } else {
                            return false.into_ffi_val();
                        }
                    }

                    true.into_ffi_val()
                })
            },
        )
        .register_fn(
            "vte/iter-cell-bg-set-attr!",
            |term: &VirtualTerminal, val: FFIArg| {
                guard_ffi("vte/iter-cell-bg-set-attr!", move || {
                    if let Some(cell) = &term.last_cell {
                        if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = val {
                            if let Some(attr) =
                                as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                            {
                                attr.0 = cell.attrs().to_owned().background();

                                true.into_ffi_val()
                            } else {
                                false.into_ffi_val()
                            }
                        } else {
                            false.into_ffi_val()
                        }
                    } else {
                        false.into_ffi_val()
                    }
                })
            },
        )
        .register_fn(
            "vte/iter-cell-fg-set-attr!",
            |term: &VirtualTerminal, val: FFIArg| {
                guard_ffi("vte/iter-cell-fg-set-attr!", move || {
                    if let Some(cell) = &term.last_cell {
                        if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = val {
                            if let Some(attr) =
                                as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                            {
                                attr.0 = cell.attrs().to_owned().foreground();

                                true.into_ffi_val()
                            } else {
                                false.into_ffi_val()
                            }
                        } else {
                            false.into_ffi_val()
                        }
                    } else {
                        false.into_ffi_val()
                    }
                })
            },
        )
        .register_fn("vte/iter-cell-str", |term: &VirtualTerminal| {
            guard_ffi("vte/iter-cell-str", move || {
                if let Some(cell) = &term.last_cell {
                    cell.str().to_string().into_ffi_val()
                } else {
                    false.into_ffi_val()
                }
            })
        })
        .register_fn(
            "vte/iter-cell-str-set-str!",
            |term: &VirtualTerminal, mut mut_str: RMut<'_, RString>| {
                guard_ffi("vte/iter-cell-str-set-str!", move || {
                    if let Some(cell) = &term.last_cell {
                        mut_str.get_mut().clear();
                        mut_str.get_mut().push_str(cell.str());

                        RResult::ROk(FFIValue::Void)
                    } else {
                        false.into_ffi_val()
                    }
                })
            },
        )
        // Batch all of the updates, less round trips across the FFI barrier
        .register_fn(
            "vte/iter-cell-bg-fg-set-attr-str!",
            |term: &VirtualTerminal, mut mut_str: RMut<'_, RString>, bg: FFIArg, fg: FFIArg| {
                guard_ffi("vte/iter-cell-bg-fg-set-attr-str!", move || {
                    if let Some(cell) = &term.last_cell {
                        mut_str.get_mut().clear();
                        mut_str.get_mut().push_str(cell.str());

                        if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = bg {
                            if let Some(attr) =
                                as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                            {
                                attr.0 = cell.attrs().background();
                            }
                        }

                        if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = fg {
                            if let Some(attr) =
                                as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut())
                            {
                                attr.0 = cell.attrs().foreground();

                                return true.into_ffi_val();
                            } else {
                                return false.into_ffi_val();
                            }
                        } else {
                            return false.into_ffi_val();
                        }
                    }

                    true.into_ffi_val()
                })
            },
        )
        .register_fn("vte/scroll-up", |term: &mut VirtualTerminal| {
            guard_ffi("vte/scroll-up", move || {
                term.scroll_up_modifier = (term.scroll_up_modifier - 1)
                    .max(0 - term.terminal.screen().scrollback_rows() as i64);
            })
        })
        .register_fn("vte/scroll-down", |term: &mut VirtualTerminal| {
            guard_ffi("vte/scroll-down", move || {
                term.scroll_up_modifier = (term.scroll_up_modifier + 1).min(0);
            })
        })
        // Test hook: always fails with a catchable Scheme error.
        .register_fn("debug-panic!", |message: &str| {
            guard_ffi("debug-panic!", move || -> FFIValue { panic!("{}", message) })
        });

    module
}

/// True for cells the renderer would paint as nothing: a blank glyph with
/// default colors and no reverse video. `buffer/clear` + `block/render` have
/// already filled the popup area before the cell loop runs, so emitting these
/// is pure overhead. The predicate deliberately mirrors exactly the attributes
/// `terminal-render` consumes (glyph, fg, bg, reverse) — bold/underline/italic
/// are ignored there, so ignoring them here stays consistent.
fn is_invisible(cell: &Cell) -> bool {
    let attrs = cell.attrs();
    matches!(cell.str(), " " | "")
        && !attrs.reverse()
        && matches!(attrs.foreground(), ColorAttribute::Default)
        && matches!(attrs.background(), ColorAttribute::Default)
}

fn update_cell(cell: &Cell, mut mut_str: RMut<'_, RString>, bg: FFIArg, fg: FFIArg) {
    mut_str.get_mut().clear();
    mut_str.get_mut().push_str(cell.str());

    if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = bg {
        if let Some(attr) = as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut()) {
            attr.0 = cell.attrs().background();
        }
    }

    if let FFIArg::CustomRef(CustomRef { mut custom, .. }) = fg {
        if let Some(attr) = as_underlying_ffi_type::<TermColorAttribute>(custom.get_mut()) {
            attr.0 = cell.attrs().foreground();
        }
    }
}

struct TermColorAttribute(ColorAttribute);

impl Custom for TermColorAttribute {}

fn create_native_pty_system(command: String) -> PtyProcess {
    spawn_in_pty(CommandBuilder::new(command))
}

fn create_native_pty_system_with_args_and_cwd(
    command: String,
    args: Vec<String>,
    cwd: String,
) -> PtyProcess {
    let mut cmd = CommandBuilder::new(command);
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    spawn_in_pty(cmd)
}

fn create_native_pty_system_with_cwd(command: String, cwd: String) -> PtyProcess {
    let mut cmd = CommandBuilder::new(command);
    cmd.cwd(cwd);
    spawn_in_pty(cmd)
}

fn spawn_in_pty(mut cmd: CommandBuilder) -> PtyProcess {
    // portable-pty's CommandBuilder does not inherit the parent environment.
    // Pass Helix's env through so a directly-spawned TUI (lazygit) sees
    // TERM/COLORTERM/LANG/PATH and detects terminal capabilities the same way
    // it would when launched from a shell. Without this, selection highlighting
    // and colors render incorrectly.
    cmd.env("TERM", "xterm-256color"); // fallback, overridden by the loop if set
    for (key, value) in std::env::vars() {
        cmd.env(key, value);
    }

    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let child = pair.slave.spawn_command(cmd).unwrap();

    // Drop our copy of the slave endpoint now that the child owns one. Holding
    // it here keeps the master from ever reaching EOF when the child exits, so
    // the reader thread below would block in read() forever and no exit signal
    // could reach the plugin (see the async-try-read-line EOF arm).
    let PtyPair { master, slave } = pair;
    drop(slave);

    // Read the output in another thread.
    // This is important because it is easy to encounter a situation
    // where read/write buffers fill and block either your process
    // or the spawned process.
    let (async_sender, async_receiver) = mpsc::unbounded_channel();

    let (cancellation_token_sender, cancellation_token_receiver) = channel::<()>();

    let mut reader = master.try_clone_reader().unwrap();
    let mut writer = WriterWrapper::new(master.take_writer().unwrap());

    let writer_clone = writer.clone();

    std::thread::spawn(move || {
        // Consume the output from the child

        let mut read_buffer = [0; 65536];
        let mut carry: Vec<_> = Vec::new();

        loop {
            match reader.read(&mut read_buffer) {
                Ok(0) => break, // EOF: child exited (fixes q/panic)
                Ok(size) => {
                    carry.extend_from_slice(&read_buffer[..size]);
                    // Decode only the valid UTF-8 prefix; keep any split trailing bytes.
                    let valid = match std::str::from_utf8(&carry) {
                        Ok(_) => carry.len(),
                        Err(e) => e.valid_up_to(),
                    };
                    if valid > 0 {
                        let s = String::from_utf8_lossy(&carry[..valid]).into_owned();
                        carry.drain(..valid);
                        if async_sender.send(s).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
            match cancellation_token_receiver.try_recv() {
                Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    });

    // TODO: Perhaps, don't use Strings here and instead just
    // use byte strings directly. However I think Strings work
    // just find for now.
    let (command_sender, command_receiver) = channel::<u8>();

    {
        // Obtain the writer.
        // When the writer is dropped, EOF will be sent to
        // the program that was spawned.
        // It is important to take the writer even if you don't
        // send anything to its stdin so that EOF can be
        // generated, otherwise you risk deadlocking yourself.
        // let mut writer = pair.master.take_writer().unwrap();

        if cfg!(target_os = "macos") {
            // macOS quirk: the child and reader must be started and
            // allowed a brief grace period to run before we allow
            // the writer to drop. Otherwise, the data we send to
            // the kernel to trigger EOF is interleaved with the
            // data read by the reader! WTF!?
            // This appears to be a race condition for very short
            // lived processes on macOS.
            // I'd love to find a more deterministic solution to
            // this than sleeping.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // This example doesn't need to write anything, but if you
        // want to send data to the child, you'd set `to_write` to
        // that data and do it like this:
        // let to_write = "ls -l";
        // if !to_write.is_empty() {
        // To avoid deadlock, wrt. reading and waiting, we send
        // data to the stdin of the child in a different thread.
        std::thread::spawn(move || {
            while let Ok(command) = command_receiver.recv() {
                // Writing to the master fails (EIO) once no slave endpoint is
                // open anywhere, i.e. after the child exits: treat it as the
                // end of the writer instead of panicking a thread inside a
                // terminal application.
                if writer.write_all(&[command]).is_err() {
                    break;
                }
            }
        });
        // }
    }

    PtyProcess {
        cancellation_token_sender,
        command_sender,
        // output_receiver: rx,
        async_receiver: Arc::new(Mutex::new(async_receiver)),
        master,
        child,
        writer: writer_clone,
    }
}

// Have this virtual terminal receive
// inputs from the plugin, where the plugin
// then does the rendering logic.
// Get back this terminal in a way that rendering
// is reasonably easy.
struct VirtualTerminal {
    terminal: wezterm_term::Terminal,
    screen_iterator: ScreenCellIterator,
    scroll_up_modifier: i64,
    last_cell: Option<Cell>,
    last_reverse: bool,
    run_x: usize,
    run_y: i64,
}

struct ScreenCellIterator {
    x: usize,
    y: i64,
}

impl Custom for VirtualTerminal {}

struct TermCell {
    cell: Cell,
}

impl Custom for TermCell {}

struct TermLine {
    line: Line,
}

impl TermLine {
    // Convert the line to a string
    fn as_str(&self) -> String {
        self.line.as_str().to_string()
    }
}

impl Custom for TermLine {}

impl VirtualTerminal {
    // Keep track of the state of the terminal
    fn advance_bytes(&mut self, bytes: &str) {
        self.terminal.advance_bytes(bytes)
    }

    fn advance_bytes_char(&mut self, bytes: char) {
        self.terminal.advance_bytes([bytes as u8])
    }

    fn advance_bytes_with_carriage_return(&mut self, bytes: &str) {
        let mut buf = [0; 4];

        for char in bytes.chars() {
            match char {
                '\n' => self.terminal.advance_bytes([b'\n', b'\r']),
                c => self.terminal.advance_bytes(c.encode_utf8(&mut buf)),
            }
        }
    }

    // Resizes the terminal
    fn resize(&mut self, rows: usize, cols: usize) {
        self.terminal.resize(wezterm_term::TerminalSize {
            rows: rows as _,
            cols: cols as _,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        });
    }

    // Get the content to render
    fn lines(&mut self) -> Vec<FFIValue> {
        let screen = self.terminal.screen();

        let rows = screen.physical_rows;

        let phys_row_start = screen.phys_row(0);
        let phys_row_end = screen.phys_row(rows as i64);

        screen
            .lines_in_phys_range(phys_row_start..phys_row_end)
            .into_iter()
            .map(|line| TermLine { line }.into_ffi_val().unwrap())
            .collect()

        // let lines = screen.lines_in_phys_range()
    }

    fn cursor(&self) -> Vec<FFIValue> {
        let pos = self.terminal.cursor_pos();

        vec![
            pos.x.into_ffi_val().unwrap(),
            (pos.y as isize).into_ffi_val().unwrap(),
        ]
    }

    fn cursor_visible(&self) -> bool {
        use wezterm_surface::CursorVisibility;
        self.terminal.cursor_pos().visibility == CursorVisibility::Visible
    }

    fn cursor_x(&self) -> usize {
        self.terminal.cursor_pos().x
    }

    fn cursor_y(&self) -> isize {
        self.terminal.cursor_pos().y as isize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Drain the PTY output channel until it stays quiet, or the deadline passes.
    fn collect_output(pty: &PtyProcess, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut out = String::new();
        let mut last_data = Instant::now();

        while Instant::now() < deadline {
            let mut got_data = false;

            if let Some(mut guard) = pty.async_receiver.try_lock() {
                while let Ok(chunk) = guard.try_recv() {
                    out.push_str(&chunk);
                    got_data = true;
                }
            }

            if got_data {
                last_data = Instant::now();
            } else if !out.is_empty() && last_data.elapsed() > Duration::from_millis(200) {
                break;
            }

            std::thread::sleep(Duration::from_millis(10));
        }

        out
    }

    fn make_virtual_terminal() -> VirtualTerminal {
        VirtualTerminal {
            terminal: Terminal::new(
                wezterm_term::TerminalSize::default(),
                Arc::new(TermConfig::new()),
                "bash",
                "0.1.0",
                Box::new(std::io::empty()),
            ),
            screen_iterator: ScreenCellIterator { x: 0, y: 0 },
            scroll_up_modifier: 0,
            last_cell: None,
            last_reverse: false,
            run_x: 0,
            run_y: 0,
        }
    }

    fn cell_string(term: &mut VirtualTerminal, x: usize, y: i32) -> Option<String> {
        term.terminal
            .screen_mut()
            .get_cell_scrollback(x, y)
            .map(|cell| cell.str().to_string())
    }

    /// ISS-001: a printable non-ASCII key must reach the PTY as UTF-8.
    #[cfg(unix)]
    #[test]
    fn send_command_char_sends_multibyte_as_utf8() {
        let mut pty = spawn_in_pty(CommandBuilder::new("/bin/cat"));

        assert!(pty.send_command_char('é'));

        let out = collect_output(&pty, Duration::from_secs(5));

        // The tty echoes the input back, so the stream may hold the glyph twice.
        // What matters: the bytes are valid UTF-8 and decode to the right glyph.
        assert!(
            out.contains('é'),
            "expected 'é' in PTY output, got {:?}",
            out
        );
        assert!(
            !out.contains('\u{FFFD}'),
            "expected valid UTF-8, got replacement chars in {:?}",
            out
        );

        // ASCII stays one byte.
        assert!(pty.send_command_char('a'));

        let ascii = collect_output(&pty, Duration::from_secs(5));

        assert!(ascii.contains('a'), "expected 'a' in PTY output, got {:?}", ascii);
    }

    /// ISS-002: non-ASCII output from the child must render as the right glyph.
    #[test]
    fn advance_bytes_char_keeps_multibyte_glyph() {
        let mut term = make_virtual_terminal();

        term.advance_bytes_char('€');

        assert_eq!(
            cell_string(&mut term, 0, 0).as_deref(),
            Some("€"),
            "expected the '€' glyph in cell (0,0)"
        );
    }

    /// AC-3: ASCII output is unaffected by the encoding fix.
    #[test]
    fn advance_bytes_char_keeps_ascii_glyph() {
        let mut term = make_virtual_terminal();

        term.advance_bytes_char('a');

        assert_eq!(
            cell_string(&mut term, 0, 0).as_deref(),
            Some("a"),
            "expected the 'a' glyph in cell (0,0)"
        );
    }
}
