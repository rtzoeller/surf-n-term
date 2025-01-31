//! Unix systems specific `Terminal` implementation.
use crate::common::{env_cfg, IOQueue};
use crate::decoder::KEYBOARD_LEVEL;
use crate::encoder::ColorDepth;
use crate::image::ImageHandlerKind;
use crate::{
    decoder::{Decoder, TTYDecoder},
    encoder::{Encoder, TTYEncoder},
    error::Error,
    image::DummyImageHandler,
    terminal::{
        Size, Terminal, TerminalCommand, TerminalEvent, TerminalSize, TerminalStats, TerminalWaker,
    },
    DecMode, ImageHandler,
};
use crate::{TerminalCaps, RGBA};
use signal_hook::{
    consts::{SIGINT, SIGQUIT, SIGTERM, SIGWINCH},
    iterator::{backend::SignalDelivery, exfiltrator::SignalOnly},
};
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::{BufWriter, Cursor, Read, Write},
    os::unix::{
        io::{AsRawFd, RawFd},
        net::UnixStream,
    },
    path::Path,
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

mod nix {
    pub use libc::{winsize, TIOCGWINSZ};
    pub use nix::{
        errno::Errno,
        fcntl::{fcntl, open, FcntlArg, OFlag},
        sys::{
            select::{select, FdSet},
            stat::Mode,
            termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios},
            time::TimeVal,
        },
        unistd::{close, isatty, read, write},
        Error,
    };
}

pub struct UnixTerminal {
    tty_handle: IOHandle,
    encoder: TTYEncoder,
    write_queue: IOQueue,
    decoder: TTYDecoder,
    events_queue: VecDeque<TerminalEvent>,
    waker_read: UnixStream,
    waker: TerminalWaker,
    termios_saved: nix::Termios,
    signal_delivery: SignalDelivery<UnixStream, SignalOnly>,
    stats: TerminalStats,
    tee: Option<BufWriter<File>>,
    image_handler: Box<dyn ImageHandler + 'static>,
    capabilities: TerminalCaps,
    // if it is not None we are going to use escape sequence to detect
    // terminal size, otherwise ioctl is used.
    size: Option<TerminalSize>,
}

impl UnixTerminal {
    /// Create new terminal by opening `/dev/tty` device.
    pub fn new() -> Result<Self, Error> {
        Self::open("/dev/tty")
    }

    /// Open terminal by a given device path
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let tty_fd = nix::open(path.as_ref(), nix::OFlag::O_RDWR, nix::Mode::empty())?;
        Self::new_from_fd(tty_fd)
    }

    /// Create new terminal from raw file descriptor pointing to /dev/tty.
    pub fn new_from_fd(tty_fd: RawFd) -> Result<Self, Error> {
        let tty_handle = IOHandle::new(tty_fd);
        tty_handle.set_blocking(false)?;
        if !nix::isatty(tty_fd)? {
            return Err(Error::NotATTY);
        }

        // switching terminal into a raw mode
        // [Entering Raw Mode](https://viewsourcecode.org/snaptoken/kilo/02.enteringRawMode.html)
        let termios_saved = nix::tcgetattr(tty_fd)?;
        let mut termios = termios_saved.clone();
        nix::cfmakeraw(&mut termios);
        nix::tcsetattr(tty_fd, nix::SetArg::TCSAFLUSH, &termios)?;

        // signal delivery
        let (signal_read, signal_write) = UnixStream::pair()?;
        let signal_delivery = SignalDelivery::with_pipe(
            signal_read,
            signal_write,
            SignalOnly,
            &[SIGWINCH, SIGTERM, SIGINT, SIGQUIT],
        )?;

        // self-pipe trick to implement waker
        let (waker_read, waker_write) = UnixStream::pair()?;
        set_blocking(waker_write.as_raw_fd(), false)?;
        let waker = TerminalWaker::new(move || {
            const WAKE: &[u8] = b"\x00";
            // use write syscall instead of locking so it would be safe to use in a signal handler
            match nix::write(waker_write.as_raw_fd(), WAKE) {
                Ok(_) | Err(nix::Errno::EINTR | nix::Errno::EAGAIN) => Ok(()),
                Err(error) => Err(error.into()),
            }
        });
        set_blocking(waker_read.as_raw_fd(), false)?;

        let capabilities = TerminalCaps::default();
        let mut term = Self {
            tty_handle,
            encoder: TTYEncoder::new(capabilities.clone()),
            write_queue: Default::default(),
            decoder: TTYDecoder::new(),
            events_queue: Default::default(),
            waker_read,
            waker,
            termios_saved,
            signal_delivery,
            stats: TerminalStats::new(),
            tee: None,
            image_handler: Box::new(DummyImageHandler),
            capabilities,
            size: None,
        };

        capabilities_detect(&mut term)?;
        term.execute(TerminalCommand::KeyboardLevel(KEYBOARD_LEVEL))?;
        Ok(term)
    }

    /// Duplicate all output to specified tee file. Used for debugging.
    pub fn duplicate_output(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let file = File::create(path)?;
        self.tee = Some(BufWriter::new(file));
        Ok(())
    }

    /// Statistics collected by terminal.
    pub fn stats(&self) -> &TerminalStats {
        &self.stats
    }

    /// Get a reference an image handler
    pub fn image_handler(&mut self) -> &mut dyn ImageHandler {
        &mut self.image_handler
    }

    /// Determine terminal size with ioctl
    ///
    /// Some terminal emulators do not set pixel size, or if it goes through some
    /// kind of muxer (like `docker exec`) which might not set pixel size. So if this
    /// condition is detected we are falling back to escape sequence if it detected to
    /// work.
    fn size_ioctl(&self) -> Result<TerminalSize, Error> {
        unsafe {
            let mut winsize: nix::winsize = std::mem::zeroed();
            if libc::ioctl(self.tty_handle.as_raw_fd(), nix::TIOCGWINSZ, &mut winsize) < 0 {
                return Err(nix::Error::last().into());
            }
            Ok(TerminalSize {
                cells: Size {
                    height: winsize.ws_row as usize,
                    width: winsize.ws_col as usize,
                },
                pixels: Size {
                    height: winsize.ws_ypixel as usize,
                    width: winsize.ws_xpixel as usize,
                },
            })
        }
    }

    /// Close all descriptors free all the resources
    fn dispose(&mut self) -> Result<(), Error> {
        self.frames_drop();

        // revert descriptor to blocking mode
        self.tty_handle.set_blocking(true)?;

        // flush currently queued output and submit the epilogue
        let epilogue = [
            TerminalCommand::Face(Default::default()),
            TerminalCommand::DecModeSet {
                enable: true,
                mode: DecMode::VisibleCursor,
            },
            TerminalCommand::DecModeSet {
                enable: false,
                mode: DecMode::MouseMotions,
            },
            TerminalCommand::DecModeSet {
                enable: false,
                mode: DecMode::MouseSGR,
            },
            TerminalCommand::DecModeSet {
                enable: false,
                mode: DecMode::MouseReport,
            },
            TerminalCommand::DecModeSet {
                enable: true,
                mode: DecMode::AutoWrap,
            },
            TerminalCommand::KeyboardLevel(0),
        ];
        epilogue
            .iter()
            .try_fold((), |_, cmd| self.execute(cmd.clone()))
            .and_then(|_| {
                while !self.write_queue.is_empty() {
                    self.poll(Some(Duration::new(0, 0)))?;
                }
                Ok(())
            })
            .unwrap_or(()); // ignore write errors
        self.drain().count(); // drain pending events

        // disable signal handler
        self.signal_delivery.handle().close();

        // restore terminal settings
        nix::tcsetattr(
            self.tty_handle.as_raw_fd(),
            nix::SetArg::TCSAFLUSH,
            &self.termios_saved,
        )?;

        Ok(())
    }
}

/// Fallback way to determine terminal size if it is detected to work
/// and ioctl is not.
const GET_TERM_SIZE: &[u8] = b"\x1b[18t\x1b[14t";

/// Detect and set terminal capabilities
fn capabilities_detect(term: &mut UnixTerminal) -> Result<(), Error> {
    if let Ok("linux") | Ok("dumb") = std::env::var("TERM").as_deref() {
        // do not try to query anything on dumb terminals
        warn!("[detected] dump terminal");
        term.capabilities.depth = ColorDepth::Gray;
        term.encoder = TTYEncoder::new(term.capabilities.clone());
        return Ok(());
    }
    let mut caps = TerminalCaps::default();
    if let Ok("truecolor") | Ok("24bit") = std::env::var("COLORTERM").as_deref() {
        caps.depth = ColorDepth::TrueColor;
    }

    // drain all pending events
    term.drain().count();
    // NOTE: using `write!` here instead of execute, to not accidentally use
    //       existing configuration from passed terminal.

    // 1x1 pixel kitty image (NOTE: it will be consumed by handler if it is already set)
    write!(term, "\x1b_Ga=q,i=31,s=1,v=1,f=24;AAAA\x1b\\")?;

    // OSC - Get default background color for transparent blending
    write!(term, "\x1b]11;?\x1b\\")?;

    // Set background color with SGR, and try to get it back to
    // detect true color support https://github.com/termstandard/colors
    let face_expected = "bg=#010203".parse()?;
    write!(term, "\x1b[00;48;2;1;2;3m")?; // change background
    write!(term, "\x1bP$qm\x1b\\")?; // DECRQSS with `m` descriptor
    write!(term, "\x1b[00m")?; // reset current face

    // Detect terminal size
    // Some terminals return incomplete size info with ioctl
    term.write_all(GET_TERM_SIZE)?;

    // Detect kitty keyboard protocol support
    write!(term, "\x1b[?u")?;

    // DA1 - sync and sixel info
    // Device Attribute command is used as "sync" event, it is supported
    // by most terminals, at least in its basic form, so we expect to
    // receive a response to it. Which means it should go LAST
    write!(term, "\x1b[c")?;

    let mut image_handlers = HashSet::new();
    let mut bg: Option<RGBA> = None;
    let mut size_escape = TerminalSize::default();
    loop {
        match term.poll(Some(Duration::from_secs(1)))? {
            Some(TerminalEvent::KittyImage { .. }) => {
                debug!("[detected] kitty image protocol");
                image_handlers.insert(ImageHandlerKind::Kitty);
            }
            Some(TerminalEvent::Color { color, .. }) => {
                debug!("[detected] background color: {:?}", color);
                bg.replace(color);
            }
            Some(TerminalEvent::FaceGet(face)) => {
                if face == face_expected {
                    debug!("[detected] true color support");
                    caps.depth = ColorDepth::TrueColor;
                }
            }
            Some(TerminalEvent::DeviceAttrs(attrs)) => {
                // 4 - attribute indicates sixel support
                if attrs.contains(&4) {
                    debug!("[detected] sixel image protocol");
                    image_handlers.insert(ImageHandlerKind::Sixel);
                }
                break; // this is last "sync" event
            }
            Some(TerminalEvent::Size(size)) => {
                size_escape = size;
            }
            Some(TerminalEvent::KeyboardLevel(_)) => {
                debug!("[detect] kitty keyboard protocol");
                caps.kitty_keyboard = true;
            }
            Some(event) => {
                warn!("unexpected event during detection: {:?}", event);
                continue;
            }
            None => break,
        }
    }

    // drain terminal
    term.drain().count();

    // color depth
    if let Some(depth) = env_cfg::<ColorDepth>("depth") {
        caps.depth = depth;
    }

    // term size interface
    let size_ioctl = term.size_ioctl()?;
    if size_ioctl.pixels.is_empty() && !size_escape.pixels.is_empty() {
        warn!("[detect] fallback to escape sequence for term size detection");
        term.size = Some(size_escape);
    }

    // image handler
    let image_handler = env_cfg::<ImageHandlerKind>("image")
        .or_else(|| image_handlers.get(&ImageHandlerKind::Kitty).copied())
        .or_else(|| image_handlers.get(&ImageHandlerKind::Sixel).copied())
        .unwrap_or(ImageHandlerKind::Dummy)
        .into_image_handler(bg);

    // glyph support
    caps.glyphs = matches!(
        image_handler.kind(),
        ImageHandlerKind::Kitty | ImageHandlerKind::Sixel
    ) && !term.size()?.pixels.is_empty();

    // update terminal
    info!("capabilities: {:?}", caps);
    term.encoder = TTYEncoder::new(caps.clone());
    term.image_handler = image_handler;
    term.capabilities = caps;

    Ok(())
}

impl std::ops::Drop for UnixTerminal {
    fn drop(&mut self) {
        self.dispose().unwrap_or(())
    }
}

impl Write for UnixTerminal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_queue.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.write_queue.flush()
    }
}

impl Terminal for UnixTerminal {
    fn poll(&mut self, timeout: Option<Duration>) -> Result<Option<TerminalEvent>, Error> {
        // NOTE:
        // Only `select` reliably works with /dev/tty on MacOS, `poll` for example
        // always returns POLLNVAL.
        self.write_queue.flush()?;
        let mut read_set = nix::FdSet::new();
        let mut write_set = nix::FdSet::new();
        let tty_fd = self.tty_handle.as_raw_fd();
        let signal_fd = self.signal_delivery.get_read().as_raw_fd();
        let waker_fd = self.waker_read.as_raw_fd();

        let timeout_instant = timeout.map(|dur| Instant::now() + dur);
        let mut first_loop = true; // execute first loop even if timeout is 0
        while !self.write_queue.is_empty() || self.events_queue.is_empty() {
            // update descriptors sets
            read_set.clear();
            read_set.insert(tty_fd);
            read_set.insert(signal_fd);
            read_set.insert(waker_fd);
            write_set.clear();
            if !self.write_queue.is_empty() {
                write_set.insert(tty_fd);
            }

            // process timeout
            let mut delay = match timeout_instant {
                Some(timeout_instant) => {
                    let now = Instant::now();
                    if timeout_instant < Instant::now() {
                        if first_loop {
                            Some(timeval_from_duration(Duration::new(0, 0)))
                        } else {
                            break;
                        }
                    } else {
                        Some(timeval_from_duration(timeout_instant - now))
                    }
                }
                None => None,
            };

            // wait for descriptors
            let select = nix::select(None, &mut read_set, &mut write_set, None, &mut delay);
            match select {
                Err(nix::Errno::EINTR | nix::Errno::EAGAIN) => return Ok(None),
                Err(error) => return Err(error.into()),
                Ok(count) => tracing::trace!("select count={}", count),
            };

            // process pending output
            if write_set.contains(tty_fd) {
                let tee = self.tee.as_mut();
                let send = self.write_queue.consume_with(|slice| {
                    let size = guard_io(self.tty_handle.write(slice), 0)?;
                    tee.map(|tee| tee.write(&slice[..size])).transpose()?;
                    Ok::<_, Error>(size)
                })?;
                self.stats.send += send;
            }
            // process signals
            if read_set.contains(signal_fd) {
                for signal in self.signal_delivery.pending() {
                    match signal {
                        SIGWINCH => {
                            if self.size.is_none() {
                                self.events_queue
                                    .push_back(TerminalEvent::Resize(self.size()?));
                            } else {
                                self.write_all(GET_TERM_SIZE)?;
                            }
                        }
                        SIGTERM | SIGINT | SIGQUIT => {
                            return Err(Error::Quit);
                        }
                        _ => {}
                    }
                }
            }
            // process waker
            if read_set.contains(waker_fd) {
                let mut buf = [0u8; 1024];
                if guard_io(self.waker_read.read(&mut buf), 0)? != 0 {
                    self.events_queue.push_back(TerminalEvent::Wake);
                }
            }
            // process pending input
            if read_set.contains(tty_fd) {
                let mut buf = [0u8; 1024];
                let recv = guard_io(self.tty_handle.read(&mut buf), 0)?;
                if recv == 0 {
                    return Err(Error::Quit);
                }
                self.stats.recv += recv;
                // parse events
                let mut read_queue = Cursor::new(&buf[..recv]);
                while let Some(event) = self.decoder.decode(&mut read_queue)? {
                    if let TerminalEvent::Size(size) = event {
                        // we are using escape sequence to determine terminal resize
                        if let Some(term_size) = self.size.as_mut() {
                            *term_size = size;
                            self.events_queue.push_back(TerminalEvent::Resize(size));
                        }
                    }
                    if !self.image_handler.handle(&event)? {
                        self.events_queue.push_back(event)
                    }
                }
                // Dirty hack to extract ambiguous terminal events (such as Escape key)
                // we assume that ambiguous events are never split across reads.
                if let Some(event) = self.decoder.take() {
                    self.events_queue.push_back(event);
                }
            }

            // indicate that first loop was executed
            first_loop = false;
        }
        Ok(self.events_queue.pop_front())
    }

    fn execute(&mut self, cmd: TerminalCommand) -> Result<(), Error> {
        tracing::trace!(?cmd, "execute");
        match cmd {
            TerminalCommand::Image(img, pos) => {
                self.image_handler.draw(&mut self.write_queue, &img, pos)
            }
            TerminalCommand::ImageErase(img, pos) => {
                self.image_handler.erase(&mut self.write_queue, &img, pos)
            }
            cmd => self.encoder.encode(&mut self.write_queue, cmd),
        }
    }

    fn size(&self) -> Result<TerminalSize, Error> {
        match self.size {
            Some(size) => Ok(size),
            None => self.size_ioctl(),
        }
    }

    fn waker(&self) -> TerminalWaker {
        self.waker.clone()
    }

    fn frames_pending(&self) -> usize {
        self.write_queue.chunks_count()
    }

    fn frames_drop(&mut self) {
        self.write_queue.clear_but_last()
    }

    fn dyn_ref(&mut self) -> &mut dyn Terminal {
        self
    }

    fn capabilities(&self) -> &TerminalCaps {
        &self.capabilities
    }
}

fn guard_io<T>(result: Result<T, std::io::Error>, otherwise: T) -> Result<T, std::io::Error> {
    use std::io::ErrorKind::*;
    match result {
        Err(error) if error.kind() == WouldBlock || error.kind() == Interrupted => Ok(otherwise),
        _ => result,
    }
}

/// Enable/disable blocking io for the provided file descriptor.
fn set_blocking(fd: RawFd, blocking: bool) -> Result<(), nix::Error> {
    let mut flags = nix::OFlag::from_bits_truncate(nix::fcntl(fd, nix::FcntlArg::F_GETFL)?);
    flags.set(nix::OFlag::O_NONBLOCK, !blocking);
    nix::fcntl(fd, nix::FcntlArg::F_SETFL(flags))?;
    Ok(())
}

fn timeval_from_duration(dur: Duration) -> nix::TimeVal {
    nix::TimeVal::from(libc::timeval {
        tv_sec: dur.as_secs() as libc::clock_t, // musl complains with `libc::time_t`
        tv_usec: dur.subsec_micros() as libc::suseconds_t,
    })
}

struct IOHandle {
    fd: RawFd,
}

impl IOHandle {
    pub fn new(fd: RawFd) -> Self {
        Self { fd }
    }

    pub fn set_blocking(&self, blocking: bool) -> Result<(), nix::Error> {
        set_blocking(self.fd, blocking)
    }
}

impl Drop for IOHandle {
    fn drop(&mut self) {
        let _ = nix::close(self.fd);
    }
}

impl AsRawFd for IOHandle {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Write for IOHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        nix::write(self.fd, buf).map_err(|_| std::io::Error::last_os_error())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for IOHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        nix::read(self.fd, buf).map_err(|_| std::io::Error::last_os_error())
    }
}
