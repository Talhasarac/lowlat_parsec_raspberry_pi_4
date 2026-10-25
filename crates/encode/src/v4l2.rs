//! The Raspberry Pi 4's encoder, through the kernel's memory-to-memory video
//! interface: `bcm2835-codec-encode`, usually `/dev/video11`.
//!
//! **Nothing is converted on the host.** The Pi's graphics device cannot read
//! an untiled buffer through the compute interface, and an untiled buffer is
//! what its compositor scans out. This encoder takes thirty-two-bit RGB as it
//! is and converts it in the VideoCore on the way in, so the display's own
//! buffer is queued here by descriptor and no pixel is touched anywhere else.
//!
//! **One codec at one depth.** The device codes H.264 at eight bits and 4:2:0
//! and nothing else, so a stream that wants more is refused at construction
//! and the stream drops the preference, the same as on any other part that
//! cannot serve it.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::Poll;

/// What the device is called by the kernel, which is how it is found.
const NAME: &str = "bcm2835-codec-encode";

/// How many coded pictures the device can hold for collection at once.
///
/// One is handed out at a time and requeued on the next call, so this is the
/// collect-side slack the encoder gets while the loop is busy elsewhere.
const BITSTREAM_BUFFERS: u32 = 4;

/// The rate the device refuses to go above, and the step it moves in.
const MAX_BITRATE: u32 = 25_000_000;
const MIN_BITRATE: u32 = 25_000;
const BITRATE_STEP: u32 = 25_000;

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No encoder of this kind on this machine.
    NoDevice,
    /// The device is there and would not open, with the system's error
    /// number: usually a sandbox or a permission, not missing hardware.
    Open(i32),
    /// A request to the device failed, named, with the system's error number.
    Ioctl(&'static str, i32),
    /// The device would not take the picture at the size or row length the
    /// display has, or the buffer is smaller than it reads.
    UnsupportedLayout,
    /// The display scans out a layout this encoder has no name for.
    UnsupportedFormat(u32),
    /// As many pictures are in flight as the device takes. Back pressure,
    /// not a fault.
    QueueFull,
    /// Pictures in system memory are not taken: this encoder reads the
    /// display's buffer, and nothing here allocates one it could copy into.
    SystemMemory,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no {NAME} device on this machine"),
            Self::Open(errno) => write!(
                f,
                "{NAME} would not open, {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::Ioctl(what, errno) => write!(
                f,
                "the encoder refused {what}, {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::UnsupportedLayout => {
                f.write_str("the encoder does not take the display's size or row length")
            }
            Self::UnsupportedFormat(fourcc) => {
                let bytes = fourcc.to_le_bytes();
                write!(
                    f,
                    "the encoder has no name for the display's layout {}",
                    String::from_utf8_lossy(&bytes)
                )
            }
            Self::QueueFull => f.write_str("every input the encoder takes is in flight"),
            Self::SystemMemory => f.write_str("this encoder takes the display's buffer only"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

/// How the encoder is built.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// The quantiser the encoder may not go below.
    pub min_qp: u32,
    /// The scanout's layout, by the display interface's four-character code.
    pub drm_format: u32,
    /// Bytes per row of the scanout buffer.
    pub pitch: u32,
    /// How many display buffers may be queued at once.
    pub depth: usize,
}

/// The device's name for a scanout layout, by the display interface's code.
///
/// **Named by what the device does, which is not what its names say.** The
/// display's "XR24" is a little-endian word with blue in the low byte, which
/// the kernel's vocabulary calls "BGR4" -- and this device reads "BGR4" with red
/// in the low byte. Measured on a Pi 4 (kernel 6.18): a red bar queued as
/// "BGR4" in the display's byte order decodes blue and a blue one red, while
/// "AB24" decodes both right. So each display layout is given the other name.
/// The unused or alpha byte is ignored either way, and anything deeper or
/// planar has no name here.
///
/// **The matrix is the device's too, and it is BT.601.** Whatever colour
/// description the format carries, the conversion inside the encoder codes
/// BT.601 at the limited range while the stream says BT.709, so saturated
/// greens and reds decode slightly off; measured the same with every
/// description the format offers.
pub fn input_of(drm_format: u32) -> Option<u32> {
    match &drm_format.to_le_bytes() {
        b"XR24" | b"AR24" => Some(fourcc(b"AB24")),
        b"XB24" | b"AB24" => Some(fourcc(b"BGR4")),
        _ => None,
    }
}

/// The encoder.
pub struct Encoder {
    fd: OwnedFd,
    /// The coded-picture buffers, mapped once.
    bitstream: Vec<Mapped>,
    /// The coded picture the last poll handed out, requeued on the next call.
    lent: Option<u32>,
    /// Which display buffer each input slot last carried, by identity.
    ///
    /// **A slot keeps its buffer when it can.** The kernel maps a buffer the
    /// first time a slot carries it and keeps the mapping while the slot keeps
    /// getting the same one, so the display's rotation lands on the same slots
    /// turn after turn instead of being mapped again every frame.
    slots: Vec<Option<u64>>,
    /// Input slots queued and not yet given back, oldest first.
    queued: VecDeque<u32>,
    /// What the device reads from each input, in bytes.
    input_size: u32,
    bitrate: u32,
    /// Counts submitted pictures, for the timestamp the device carries across.
    sequence: u64,
}

impl core::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("slots", &self.slots.len())
            .field("queued", &self.queued.len())
            .field("bitrate", &self.bitrate)
            .finish_non_exhaustive()
    }
}

impl Encoder {
    /// Whether this machine has the device at all, without opening it.
    pub fn present() -> bool {
        find().is_some()
    }

    /// Find the device and build an encoder for `config`.
    pub fn open(config: &Config) -> Result<Self> {
        let input =
            input_of(config.drm_format).ok_or(Error::UnsupportedFormat(config.drm_format))?;
        let path = find().ok_or(Error::NoDevice)?;
        let path = std::ffi::CString::new(path.into_os_string().into_encoded_bytes())
            .map_err(|_| Error::NoDevice)?;
        // SAFETY: a path we own and flags the call understands.
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(Error::Open(errno()));
        }
        // SAFETY: just opened and owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let mut caps = Capability::default();
        ioctl(&fd, VIDIOC_QUERYCAP, &mut caps, "its capabilities")?;
        if caps.device_caps & CAP_VIDEO_M2M_MPLANE == 0 {
            return Err(Error::NoDevice);
        }

        // **The coded side first**, which is the order the interface asks for:
        // the coded format decides what the raw side may be.
        let mut coded = Format::mplane(
            BUF_TYPE_CAPTURE_MPLANE,
            PixFormatMplane {
                width: config.width,
                height: config.height,
                pixelformat: fourcc(b"H264"),
                field: FIELD_NONE,
                num_planes: 1,
                plane_fmt: one_plane(0, (config.width * config.height).max(512 * 1024)),
                ..PixFormatMplane::default()
            },
        );
        ioctl(&fd, VIDIOC_S_FMT, &mut coded, "the coded format")?;

        let mut raw_format = Format::mplane(
            BUF_TYPE_OUTPUT_MPLANE,
            PixFormatMplane {
                width: config.width,
                height: config.height,
                pixelformat: input,
                field: FIELD_NONE,
                num_planes: 1,
                plane_fmt: one_plane(config.pitch, 0),
                // **The range and the matrix the device converts with.** Its
                // own conversion runs inside the encoder, so this is where the
                // colour the guest decodes is decided.
                colorspace: COLORSPACE_REC709,
                ycbcr_enc: YCBCR_ENC_709,
                quantization: QUANTIZATION_LIM_RANGE,
                xfer_func: XFER_FUNC_709,
                ..PixFormatMplane::default()
            },
        );
        ioctl(&fd, VIDIOC_S_FMT, &mut raw_format, "the picture format")?;
        let pix = raw_format.pix();
        let taken = pix.pixelformat;
        let first = pix.plane_fmt;
        let (row, input_size) = (first[0].bytesperline, first[0].sizeimage);
        // **Refused rather than adjusted.** The device answers a layout it
        // cannot take with one it can, and reading the display's buffer at a
        // row length it does not have is a sheared picture, not an error.
        if taken != input || row != config.pitch {
            return Err(Error::UnsupportedLayout);
        }

        let mut rate = StreamParm {
            type_: BUF_TYPE_OUTPUT_MPLANE,
            ..StreamParm::default()
        };
        rate.timeperframe = Fract {
            numerator: 1,
            denominator: config.fps.max(1),
        };
        // A rate the device will not take only costs its rate control a guess.
        if let Err(error) = ioctl(&fd, VIDIOC_S_PARM, &mut rate, "the frame rate") {
            lowlat_common::log_warn!("encode: {error}");
        }

        let bitrate = clamp_bitrate(config.bitrate_bps);
        // **The ones that shape the stream are required**; the rest are
        // preferences a firmware may not have, and a missing one is said once.
        // **Variable rate, and not by preference.** The constant-rate mode is
        // accepted as a control and then refused when the input is enabled:
        // measured on a Pi 4 with kernel 6.18, every start fails with "No such
        // process" and the kernel logs a warning. The congestion controller
        // moves the target through `reconfigure` either way.
        set(&fd, CID_BITRATE_MODE, BITRATE_MODE_VBR, "variable rate")?;
        set(
            &fd,
            CID_BITRATE,
            i32::try_from(bitrate).unwrap_or(i32::MAX),
            "the bitrate",
        )?;
        set(&fd, CID_B_FRAMES, 0, "no reordered pictures")?;
        for (id, value, what) in [
            // Refreshes on demand only; a periodic one is a burst nobody asked
            // for, and a guest that needs one says so.
            (CID_GOP_SIZE, i32::MAX, "an open-ended group"),
            (CID_H264_I_PERIOD, i32::MAX, "no periodic refresh"),
            // The parameter sets travel with every refresh, which is what lets
            // a guest that joins late start decoding at the next one.
            (CID_HEADER_MODE, HEADER_MODE_JOINED, "inline parameter sets"),
            (CID_REPEAT_SEQ_HEADER, 1, "repeated parameter sets"),
            (CID_H264_PROFILE, PROFILE_HIGH, "the high profile"),
            (CID_H264_LEVEL, LEVEL_4_2, "level 4.2"),
            (
                CID_H264_MIN_QP,
                i32::try_from(config.min_qp.min(51)).unwrap_or(0),
                "the quantiser floor",
            ),
        ] {
            if let Err(error) = set(&fd, id, value, what) {
                lowlat_common::log_warn!("encode: {error}");
            }
        }

        let mut inputs = RequestBuffers {
            count: u32::try_from(config.depth).unwrap_or(4),
            type_: BUF_TYPE_OUTPUT_MPLANE,
            memory: MEMORY_DMABUF,
            ..RequestBuffers::default()
        };
        ioctl(&fd, VIDIOC_REQBUFS, &mut inputs, "the picture inputs")?;
        let mut outputs = RequestBuffers {
            count: BITSTREAM_BUFFERS,
            type_: BUF_TYPE_CAPTURE_MPLANE,
            memory: MEMORY_MMAP,
            ..RequestBuffers::default()
        };
        ioctl(&fd, VIDIOC_REQBUFS, &mut outputs, "the coded outputs")?;

        let mut bitstream = Vec::with_capacity(outputs.count as usize);
        for index in 0..outputs.count {
            let mut planes = [Plane::default(); 1];
            let mut buffer =
                Buffer::mplane(index, BUF_TYPE_CAPTURE_MPLANE, MEMORY_MMAP, &mut planes);
            ioctl(&fd, VIDIOC_QUERYBUF, &mut buffer, "a coded output")?;
            let length = planes[0].length as usize;
            // SAFETY: the offset is a mem_offset the kernel handed out for
            // this buffer.
            let offset = unsafe { planes[0].m.mem_offset };
            // SAFETY: a fresh shared mapping of a buffer the device owns; the
            // length and offset are the kernel's own.
            let ptr = unsafe {
                libc::mmap(
                    core::ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    libc::off_t::from(offset),
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(Error::Ioctl("a coded output's mapping", errno()));
            }
            bitstream.push(Mapped {
                ptr: ptr.cast(),
                len: length,
            });
        }

        let mut encoder = Self {
            fd,
            bitstream,
            lent: None,
            slots: vec![None; inputs.count as usize],
            queued: VecDeque::with_capacity(inputs.count as usize),
            input_size,
            bitrate,
            sequence: 0,
        };
        for index in 0..outputs.count {
            encoder.requeue(index)?;
        }
        for kind in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
            let mut kind = kind;
            ioctl(&encoder.fd, VIDIOC_STREAMON, &mut kind, "to start")?;
        }
        lowlat_common::log_info!(
            "encode: {NAME}, {}x{} from {} at {} bytes a row, {} inputs, {:.1} Mbit/s",
            config.width,
            config.height,
            String::from_utf8_lossy(&input.to_le_bytes()),
            config.pitch,
            encoder.slots.len(),
            f64::from(bitrate) / 1e6
        );
        Ok(encoder)
    }

    /// How many display buffers may be queued at once.
    pub fn depth(&self) -> usize {
        self.slots.len()
    }

    /// Queue one display buffer, by descriptor, under the identity that says
    /// which buffer it is.
    ///
    /// **The descriptor is not kept.** The kernel takes its own reference when
    /// the buffer is queued, so the caller may close it at any time after this
    /// returns.
    pub fn submit_buffer(&mut self, key: u64, fd: RawFd, force_keyframe: bool) -> Result<()> {
        self.reclaim()?;
        if self.queued.len() >= self.slots.len() {
            return Err(Error::QueueFull);
        }
        let free: Vec<u32> = (0..u32::try_from(self.slots.len()).unwrap_or(u32::MAX))
            .filter(|at| !self.queued.contains(at))
            .collect();
        let carried = |at: &&u32| self.slots[**at as usize];
        let index = *free
            .iter()
            .find(|at| carried(at) == Some(key))
            .or_else(|| free.iter().find(|at| carried(at).is_none()))
            .or_else(|| free.first())
            .ok_or(Error::QueueFull)?;

        // **The buffer has to cover what the device reads.** A shorter one is
        // refused by the kernel with an answer that names nothing.
        // SAFETY: a seek on a descriptor the caller holds; it moves nothing a
        // reader depends on, since a buffer's descriptor has no file position
        // anyone reads through.
        let size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        if size >= 0 && size < i64::from(self.input_size) {
            return Err(Error::UnsupportedLayout);
        }

        if force_keyframe {
            set(&self.fd, CID_FORCE_KEY_FRAME, 1, "a refresh")?;
        }
        let mut planes = [Plane {
            bytesused: self.input_size,
            length: u32::try_from(size.max(0)).unwrap_or(0),
            m: PlaneM { fd },
            ..Plane::default()
        }];
        let mut buffer = Buffer::mplane(index, BUF_TYPE_OUTPUT_MPLANE, MEMORY_DMABUF, &mut planes);
        buffer.timestamp = libc::timeval {
            tv_sec: i64::try_from(self.sequence / 1_000_000).unwrap_or(0),
            tv_usec: i64::try_from(self.sequence % 1_000_000).unwrap_or(0),
        };
        buffer.flags = BUF_FLAG_TIMESTAMP_COPY;
        ioctl(&self.fd, VIDIOC_QBUF, &mut buffer, "a picture")?;
        self.sequence = self.sequence.wrapping_add(1);
        self.slots[index as usize] = Some(key);
        self.queued.push_back(index);
        Ok(())
    }

    /// Take back every input the device has finished reading.
    fn reclaim(&mut self) -> Result<()> {
        loop {
            let mut planes = [Plane::default(); 1];
            let mut buffer = Buffer::mplane(0, BUF_TYPE_OUTPUT_MPLANE, MEMORY_DMABUF, &mut planes);
            match ioctl(&self.fd, VIDIOC_DQBUF, &mut buffer, "a finished picture") {
                Ok(()) => {
                    let index = buffer.index;
                    self.queued.retain(|at| *at != index);
                }
                Err(Error::Ioctl(_, libc::EAGAIN)) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    /// Give a coded-picture buffer back to the device.
    fn requeue(&mut self, index: u32) -> Result<()> {
        let mut planes = [Plane::default(); 1];
        planes[0].length = u32::try_from(self.bitstream[index as usize].len).unwrap_or(u32::MAX);
        let mut buffer = Buffer::mplane(index, BUF_TYPE_CAPTURE_MPLANE, MEMORY_MMAP, &mut planes);
        ioctl(&self.fd, VIDIOC_QBUF, &mut buffer, "a coded output")
    }
}

impl crate::Encoder for Encoder {
    type Error = Error;

    fn submit(&mut self, _frame: &lowlat_capture::Frame<'_>, _force_keyframe: bool) -> Result<()> {
        Err(Error::SystemMemory)
    }

    fn poll(&mut self) -> Result<Poll<'_>> {
        if let Some(index) = self.lent.take() {
            self.requeue(index)?;
        }
        self.reclaim()?;
        loop {
            let mut planes = [Plane::default(); 1];
            let mut buffer = Buffer::mplane(0, BUF_TYPE_CAPTURE_MPLANE, MEMORY_MMAP, &mut planes);
            match ioctl(&self.fd, VIDIOC_DQBUF, &mut buffer, "a coded picture") {
                Ok(()) => {}
                Err(Error::Ioctl(_, libc::EAGAIN)) => return Ok(Poll::Pending),
                Err(error) => return Err(error),
            }
            let index = buffer.index;
            let used = planes[0].bytesused as usize;
            let from = planes[0].data_offset as usize;
            let (ptr, len) = {
                let mapped = &self.bitstream[index as usize];
                (mapped.ptr, mapped.len)
            };
            // **An empty or failed picture is given straight back.** The
            // device reports one when it drops a frame, and handing it on would
            // be a unit the packetiser sends as nothing.
            if buffer.flags & BUF_FLAG_ERROR != 0 || used <= from || used > len {
                self.requeue(index)?;
                continue;
            }
            self.lent = Some(index);
            // SAFETY: the mapping lives as long as the encoder, the device has
            // handed this buffer back and does not write it again until it is
            // requeued, which the borrow on `self` prevents until the slice is
            // gone.
            let bitstream = unsafe { core::slice::from_raw_parts(ptr.add(from), used - from) };
            let keyframe = buffer.flags & BUF_FLAG_KEYFRAME != 0 || has_idr(bitstream);
            return Ok(Poll::Ready {
                bitstream,
                keyframe,
            });
        }
    }

    fn reconfigure(&mut self, bitrate_bps: u32) -> Result<()> {
        let bitrate = clamp_bitrate(bitrate_bps);
        if bitrate == self.bitrate {
            return Ok(());
        }
        set(
            &self.fd,
            CID_BITRATE,
            i32::try_from(bitrate).unwrap_or(i32::MAX),
            "the bitrate",
        )?;
        self.bitrate = bitrate;
        Ok(())
    }

    fn wait_finished(&mut self, limit: core::time::Duration) -> bool {
        let mut ready = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(limit.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one descriptor this encoder owns, polled for reading.
        let woke = unsafe { libc::poll(&raw mut ready, 1, ms) };
        woke > 0 && ready.revents & libc::POLLIN != 0
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        for kind in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
            let mut kind = kind;
            let _ = ioctl(&self.fd, VIDIOC_STREAMOFF, &mut kind, "to stop");
        }
        for mapped in self.bitstream.drain(..) {
            // SAFETY: mapped in `open` with this length and unmapped once.
            unsafe { libc::munmap(mapped.ptr.cast(), mapped.len) };
        }
    }
}

/// Whether a coded picture carries an instantaneous refresh.
///
/// **Asked of the bytes as well as the flag**, because the flag is the
/// firmware's to set and a guest skipping to a refresh must not wait for one
/// that went out unmarked.
fn has_idr(unit: &[u8]) -> bool {
    unit.windows(4)
        .any(|at| at[..3] == [0, 0, 1] && at[3] & 0x1f == 5)
}

/// A bitrate the device takes: inside its range, on its step.
fn clamp_bitrate(bps: u32) -> u32 {
    let clamped = bps.clamp(MIN_BITRATE, MAX_BITRATE);
    clamped - clamped % BITRATE_STEP
}

/// The encoder's node, found by the name the kernel gives it.
fn find() -> Option<std::path::PathBuf> {
    let mut nodes: Vec<_> = std::fs::read_dir("/sys/class/video4linux")
        .ok()?
        .flatten()
        .collect();
    nodes.sort_by_key(std::fs::DirEntry::file_name);
    nodes.into_iter().find_map(|node| {
        let name = std::fs::read_to_string(node.path().join("name")).ok()?;
        (name.trim() == NAME).then(|| std::path::Path::new("/dev").join(node.file_name()))
    })
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// One request, answered with the call it was and the reason it failed.
fn ioctl<T>(fd: &OwnedFd, request: libc::c_ulong, arg: &mut T, what: &'static str) -> Result<()> {
    loop {
        // SAFETY: `arg` is the structure this request is defined over, laid out
        // as the kernel's (checked by size below), and outlives the call.
        let done = unsafe { libc::ioctl(fd.as_raw_fd(), request, core::ptr::from_mut(arg)) };
        if done >= 0 {
            return Ok(());
        }
        let errno = errno();
        if errno != libc::EINTR {
            return Err(Error::Ioctl(what, errno));
        }
    }
}

fn set(fd: &OwnedFd, id: u32, value: i32, what: &'static str) -> Result<()> {
    let mut control = Control { id, value };
    ioctl(fd, VIDIOC_S_CTRL, &mut control, what)
}

/// A coded-picture buffer, mapped.
struct Mapped {
    ptr: *mut u8,
    len: usize,
}

// The interface's own numbers, from videodev2.h and v4l2-controls.h. None of
// them can change.

const fn request(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    // The size is a handful of bytes and the field holds fourteen bits.
    #[expect(clippy::cast_possible_truncation, reason = "sizes below 16 KiB")]
    let size = size as u32;
    ((dir << 30) | (size << 16) | ((b'V' as u32) << 8) | nr) as libc::c_ulong
}

const READ: u32 = 2;
const WRITE: u32 = 1;
const BOTH: u32 = 3;

const VIDIOC_QUERYCAP: libc::c_ulong = request(READ, 0, size_of::<Capability>());
const VIDIOC_S_FMT: libc::c_ulong = request(BOTH, 5, size_of::<Format>());
const VIDIOC_REQBUFS: libc::c_ulong = request(BOTH, 8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: libc::c_ulong = request(BOTH, 9, size_of::<Buffer>());
const VIDIOC_QBUF: libc::c_ulong = request(BOTH, 15, size_of::<Buffer>());
const VIDIOC_DQBUF: libc::c_ulong = request(BOTH, 17, size_of::<Buffer>());
const VIDIOC_STREAMON: libc::c_ulong = request(WRITE, 18, size_of::<u32>());
const VIDIOC_STREAMOFF: libc::c_ulong = request(WRITE, 19, size_of::<u32>());
const VIDIOC_S_PARM: libc::c_ulong = request(BOTH, 22, size_of::<StreamParm>());
const VIDIOC_S_CTRL: libc::c_ulong = request(BOTH, 28, size_of::<Control>());

const CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
const BUF_TYPE_CAPTURE_MPLANE: u32 = 9;
const BUF_TYPE_OUTPUT_MPLANE: u32 = 10;
const MEMORY_MMAP: u32 = 1;
const MEMORY_DMABUF: u32 = 4;
const FIELD_NONE: u32 = 1;
const BUF_FLAG_KEYFRAME: u32 = 0x0000_0008;
const BUF_FLAG_ERROR: u32 = 0x0000_0040;
const BUF_FLAG_TIMESTAMP_COPY: u32 = 0x0000_4000;
const COLORSPACE_REC709: u32 = 3;
const YCBCR_ENC_709: u8 = 2;
const QUANTIZATION_LIM_RANGE: u8 = 2;
const XFER_FUNC_709: u8 = 1;

const CID_CODEC_BASE: u32 = 0x0099_0900;
const CID_B_FRAMES: u32 = CID_CODEC_BASE + 202;
const CID_GOP_SIZE: u32 = CID_CODEC_BASE + 203;
const CID_BITRATE_MODE: u32 = CID_CODEC_BASE + 206;
const CID_BITRATE: u32 = CID_CODEC_BASE + 207;
const CID_HEADER_MODE: u32 = CID_CODEC_BASE + 216;
const CID_REPEAT_SEQ_HEADER: u32 = CID_CODEC_BASE + 226;
const CID_FORCE_KEY_FRAME: u32 = CID_CODEC_BASE + 229;
const CID_H264_MIN_QP: u32 = CID_CODEC_BASE + 353;
const CID_H264_I_PERIOD: u32 = CID_CODEC_BASE + 358;
const CID_H264_LEVEL: u32 = CID_CODEC_BASE + 359;
const CID_H264_PROFILE: u32 = CID_CODEC_BASE + 363;
const BITRATE_MODE_VBR: i32 = 0;
const HEADER_MODE_JOINED: i32 = 1;
const PROFILE_HIGH: i32 = 4;
const LEVEL_4_2: i32 = 13;

#[repr(C)]
#[derive(Default)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct PlanePixFormat {
    sizeimage: u32,
    bytesperline: u32,
    reserved: [u16; 6],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct PixFormatMplane {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    colorspace: u32,
    plane_fmt: [PlanePixFormat; 8],
    num_planes: u8,
    flags: u8,
    ycbcr_enc: u8,
    quantization: u8,
    xfer_func: u8,
    reserved: [u8; 7],
}

/// The format union, sized and aligned as the kernel's: its largest member
/// carries pointers.
#[repr(C)]
union FormatUnion {
    pix_mp: PixFormatMplane,
    raw: [u64; 25],
}

#[repr(C)]
struct Format {
    type_: u32,
    fmt: FormatUnion,
}

impl Format {
    fn mplane(type_: u32, pix_mp: PixFormatMplane) -> Self {
        let mut fmt = FormatUnion { raw: [0; 25] };
        fmt.pix_mp = pix_mp;
        Self { type_, fmt }
    }

    /// The multi-plane member, by value: it is packed, so it is never
    /// borrowed.
    fn pix(&self) -> PixFormatMplane {
        // SAFETY: every bit pattern is a valid multi-plane format, the union
        // was zeroed whole when it was made, and the kernel writes this member.
        unsafe { self.fmt.pix_mp }
    }
}

/// One plane's row length and size, the rest of the planes unused.
fn one_plane(bytesperline: u32, sizeimage: u32) -> [PlanePixFormat; 8] {
    let mut planes = [PlanePixFormat::default(); 8];
    planes[0] = PlanePixFormat {
        sizeimage,
        bytesperline,
        reserved: [0; 6],
    };
    planes
}

#[repr(C)]
#[derive(Default)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
union PlaneM {
    mem_offset: u32,
    userptr: libc::c_ulong,
    fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Plane {
    bytesused: u32,
    length: u32,
    m: PlaneM,
    data_offset: u32,
    reserved: [u32; 11],
}

impl Default for Plane {
    fn default() -> Self {
        Self {
            bytesused: 0,
            length: 0,
            m: PlaneM { userptr: 0 },
            data_offset: 0,
            reserved: [0; 11],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
union BufferM {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut Plane,
    fd: i32,
}

#[repr(C)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: BufferM,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

impl Buffer {
    /// A multi-plane buffer description over `planes`, which must outlive
    /// every request made with it.
    fn mplane(index: u32, type_: u32, memory: u32, planes: &mut [Plane]) -> Self {
        Self {
            index,
            type_,
            bytesused: 0,
            flags: 0,
            field: FIELD_NONE,
            timestamp: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            timecode: Timecode::default(),
            sequence: 0,
            memory,
            m: BufferM {
                planes: planes.as_mut_ptr(),
            },
            length: u32::try_from(planes.len()).unwrap_or(1),
            reserved2: 0,
            request_fd: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

/// The output side's stream parameters, padded to the kernel's union.
#[repr(C)]
#[derive(Default)]
struct StreamParm {
    type_: u32,
    capability: u32,
    outputmode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    writebuffers: u32,
    reserved: [u32; 4],
    padding: [[u32; 8]; 5],
}

#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

// The layouts above are the kernel's on a 64-bit machine, which is every Pi
// this runs on; a field out of place changes a size before it changes a
// picture.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<PixFormatMplane>() == 192);
    assert!(size_of::<Format>() == 208);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Plane>() == 64);
    assert!(size_of::<Buffer>() == 88);
    assert!(size_of::<StreamParm>() == 204);
    assert!(size_of::<Control>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_scanout_layout_gets_the_name_the_device_reads_it_by() {
        assert_eq!(input_of(fourcc(b"XR24")), Some(fourcc(b"AB24")));
        assert_eq!(input_of(fourcc(b"AR24")), Some(fourcc(b"AB24")));
        assert_eq!(input_of(fourcc(b"XB24")), Some(fourcc(b"BGR4")));
        assert_eq!(input_of(fourcc(b"XR30")), None);
        assert_eq!(input_of(fourcc(b"NV12")), None);
    }

    #[test]
    fn a_bitrate_lands_inside_the_range_on_the_step() {
        assert_eq!(clamp_bitrate(0), MIN_BITRATE);
        assert_eq!(clamp_bitrate(u32::MAX), MAX_BITRATE);
        assert_eq!(clamp_bitrate(10_012_345), 10_000_000);
    }

    #[test]
    fn a_refresh_is_found_in_the_bytes() {
        let sps_pps_idr = [
            0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0x88,
        ];
        assert!(has_idr(&sps_pps_idr));
        let predicted = [0, 0, 0, 1, 0x41, 0x9a, 0x02];
        assert!(!has_idr(&predicted));
    }

    #[test]
    fn the_requests_carry_the_kernel_numbers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
    }
}
