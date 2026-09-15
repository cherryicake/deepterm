//! 命令执行：动作分发、分片累积、响应生成。

use super::animation::{
    self, Animation, Background, CompositionMode, DEFAULT_GAP_MS, Frame, State,
};
use super::command::{
    Action, Command, Compression, DeleteTarget, Format, Medium, ParseError, Quiet, decode_base64,
};
use super::image::{self, DecodeError, ImageId};
use super::storage::{Placement, PlacementLocation, Storage};

/// 一次执行的产出。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Response {
    /// 需要回写给 PTY 的字节（应答 `ESC _ G ... ESC \`）。
    pub reply: Vec<u8>,
    /// 图像集合是否发生变化（需要重绘）。
    pub images_changed: bool,
}

/// 执行错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionError {
    /// 解析失败。
    Parse(ParseError),
    /// 解码失败。
    Decode(DecodeError),
    /// 介质不受支持（文件 / 共享内存被策略关闭）。
    UnsupportedMedium,
    /// 负载不是合法的文件路径，或文件无法读取（对齐 ghostty `InvalidData`）。
    InvalidData,
    /// `t=t` 的路径不在允许的临时目录内。
    TemporaryFileNotInTempDir,
    /// `t=t` 的文件名不含 `tty-graphics-protocol`。
    TemporaryFileNotNamedCorrectly,
    /// 分片状态错乱（未开始的续传、id 不匹配等）。
    InvalidChunk,
    /// 动画协议错误：带自定义 kitty 错误消息。
    ///
    /// 动画的错误码种类多（`ENOENT`/`ENOSPC`/`EINVAL` 各有若干具体原因），
    /// 用固定枚举会退化掉客户端排查所需的信息，因此这里直接携带文本。
    Animation(&'static str),
}

impl ExecutionError {
    /// kitty 响应里的错误文本。
    pub fn message(&self) -> &'static str {
        match self {
            Self::Parse(e) => e.message(),
            Self::Decode(e) => e.message(),
            Self::UnsupportedMedium => "ENOTSUP: unsupported medium",
            Self::InvalidData => "EINVAL: invalid data",
            Self::TemporaryFileNotInTempDir => "EINVAL: temporary file not in temp dir",
            Self::TemporaryFileNotNamedCorrectly => "EINVAL: temporary file not named correctly",
            Self::InvalidChunk => "EINVAL: invalid chunk",
            Self::Animation(msg) => msg,
        }
    }
}

impl From<ParseError> for ExecutionError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

impl From<DecodeError> for ExecutionError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// 进行中的分片传输。
///
/// 累积的是**未解码的 base64 文本**而非解码后的字节：kitty 规定客户端先把
/// 整个负载编码、再按 4 字节对齐切分编码文本，接收方要拼完再解码。逐片
/// 解码虽然在各片都对齐时结果相同，但规范只保证非末片对齐；按文本累积
/// 才是与规范一致的做法。
#[derive(Debug)]
struct LoadingImage {
    id: ImageId,
    number: Option<u32>,
    format: Format,
    compression: Compression,
    /// 传输介质。分片续片不带 `t=`，必须记住首片的介质才能正确取负载。
    medium: Medium,
    width: Option<u32>,
    height: Option<u32>,
    /// 已拼接的 base64 文本（各片首尾相接）。
    ///
    /// 对 `t=f`/`t=t` 而言这是**路径**的 base64，而非像素数据。
    encoded: Vec<u8>,
    /// 非 `None` 时这次加载的是**动画帧**而非整幅图像。
    frame: Option<FrameLoad>,
}

/// 正在加载的动画帧的上下文（`a=f`）。
///
/// 分片续片只携带负载，所有帧参数都在首片随本结构保存。
#[derive(Debug)]
struct FrameLoad {
    /// 帧所属图像在加载开始时的世代号。加载期间图像可能被删除、淘汰或替换，
    /// 末片按 `(id, generation)` 重新解析出**同一个**图像，否则报 ENOENT。
    image_generation: u64,
    /// `r=`：要编辑的既有帧号；0（或超出下一帧号）表示新建。
    edit_frame: u32,
    /// `c=`：新建帧时用作底画的既有帧号；0 表示用背景色填充。
    create_frame: u32,
    /// `z=`：新帧的间隔毫秒；正数直接用，负数表示无间隔，0 用默认值。
    gap_ms: i32,
    /// 帧矩形相对图像左上角的像素偏移。
    x: u32,
    y: u32,
    /// `X=`：合成模式（`1` 覆盖，其余混合）。
    composition_mode: CompositionMode,
    /// `Y=`：新建帧的底画背景色。
    background: Background,
}

/// 受支持的传输介质策略。
///
/// 默认与 ghostty 一致：文件、临时文件、共享内存全部开启
/// （见 [`MediumPolicy::all`]）。需要收紧时用 [`MediumPolicy::direct`]。
#[derive(Debug, Clone)]
pub struct MediumPolicy {
    pub allow_file: bool,
    pub allow_temporary_file: bool,
    pub allow_shared_memory: bool,
    /// `t=t` 允许读取的临时目录。仅 `allow_temporary_file` 为真时有意义。
    pub temporary_directory: Option<std::path::PathBuf>,
}

impl Default for MediumPolicy {
    fn default() -> Self {
        Self::all()
    }
}

impl MediumPolicy {
    /// 全部开启，附临时目录（对齐 ghostty `Limits.allWithTempDir`）。
    ///
    /// ghostty 在 `termio/Termio.zig:276` 用
    /// `Limits.allWithTempDir(global.tmpDirPath())` 作为**默认**装载策略，
    /// 即 `t=f`/`t=t`/`t=s` 全部可用。deepterm 早期版本把三者都默认关掉，
    /// 结果是任何用文件介质传图的客户端（例如 terminal-browser 发
    /// `a=T,f=32,t=f,i=1,C=1,q=2;<路径>`）都会被静默拒绝：`q=2` 下不产生
    /// 任何应答，屏幕上什么都不显示。
    pub fn all() -> Self {
        Self {
            allow_file: true,
            allow_temporary_file: true,
            allow_shared_memory: true,
            temporary_directory: std::env::temp_dir().canonicalize().ok(),
        }
    }

    /// 只允许直传（`t=d`），禁止一切文件与共享内存介质。
    pub fn direct() -> Self {
        Self {
            allow_file: false,
            allow_temporary_file: false,
            allow_shared_memory: false,
            temporary_directory: None,
        }
    }
}

/// 图像协议的执行器：持有存储与分片状态。
///
/// 存储用 `Arc<Mutex<..>>` 共享：本地终端的解析发生在读线程，而渲染发生在
/// UI 线程，两者必须看到同一份图像集合。分片状态（`loading`）不共享——
/// 它只对产生它的那条管线有意义（协议规定分片是顺序的）。
#[derive(Debug)]
pub struct ImageStore {
    storage: SharedStorage,
    loading: Option<LoadingImage>,
    policy: MediumPolicy,
    /// 用隐式 id 时是否需要在响应里回报 id。
    enabled: bool,
}

/// 共享的图像存储句柄。
pub type SharedStorage = std::sync::Arc<std::sync::Mutex<Storage>>;

impl ImageStore {
    /// 新建执行器。`limit_bytes` 为 0 表示禁用整个协议。
    pub fn new(limit_bytes: usize) -> Self {
        Self::from_shared(std::sync::Arc::new(std::sync::Mutex::new(Storage::new(
            limit_bytes,
        ))))
    }

    /// 在既有共享存储上新建执行器（多管线共用同一份图像集合）。
    pub fn from_shared(storage: SharedStorage) -> Self {
        let enabled = storage.lock().map(|s| s.enabled()).unwrap_or(false);
        Self {
            storage,
            loading: None,
            policy: MediumPolicy::default(),
            enabled,
        }
    }

    /// 协议是否启用。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 显式设置启用状态。
    ///
    /// 共享存储场景下无法从 `Storage` 反推「协议是否被禁用」（存储对象本身
    /// 不含该语义），因此由构造方显式告知。
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// 替换介质策略（默认见 [`MediumPolicy::all`]）。
    pub fn set_policy(&mut self, policy: MediumPolicy) {
        self.policy = policy;
    }

    /// 共享存储句柄（渲染层用）。
    pub fn shared_storage(&self) -> SharedStorage {
        self.storage.clone()
    }

    /// 只读访问存储。
    ///
    /// 回调形式而非返回引用：锁由调用方申请、在回调返回后释放，避免把
    /// `MutexGuard` 泄漏到调用点（`Mutex` 的锁不可重入，泄漏会导致死锁）。
    pub fn with_storage<R>(&self, f: impl FnOnce(&Storage) -> R) -> R {
        let guard = self.storage.lock().expect("kitty 图像存储锁被投毒");
        f(&guard)
    }

    /// 可变访问存储（回调形式，理由同上）。
    pub fn with_storage_mut<R>(&mut self, f: impl FnOnce(&mut Storage) -> R) -> R {
        let mut guard = self.storage.lock().expect("kitty 图像存储锁被投毒");
        f(&mut guard)
    }

    /// 直接取存储锁。
    ///
    /// 仅供测试：调用方拿到 `MutexGuard` 后可以随意借用（`with_storage` 的
    /// 回调形式无法返回借用，测试里读 `placements` 之类就不方便）。
    /// 生产代码请用回调形式，避免 guard 逃逸出可控范围。
    #[cfg(test)]
    pub fn lock_storage(&self) -> std::sync::MutexGuard<'_, Storage> {
        self.storage.lock().expect("kitty 图像存储锁被投毒")
    }

    /// 当前占用字节数。
    pub fn total_bytes(&self) -> usize {
        self.with_storage(|s| s.total_bytes())
    }

    /// 执行一条命令。
    ///
    /// `cursor` 为当前光标在网格中的绝对位置（行、列），用于 `d=c`/`a=T` 的锚定。
    pub fn execute(
        &mut self,
        cmd: Command,
        cursor: (i32, u32),
    ) -> Result<Response, ExecutionError> {
        if !self.enabled {
            // 协议被禁用：静默丢弃，符合 kitty 的"未启用即无响应"行为。
            return Ok(Response::default());
        }

        let quiet = cmd.quiet;
        let (mut response, succeeded) = match self.execute_inner(&cmd, cursor) {
            Ok(resp) => (resp, true),
            Err(err) => (
                Response {
                    reply: self.error_reply(err.message()),
                    images_changed: false,
                },
                false,
            ),
        };

        // 按静默级别决定是否回写响应：`q=1` 只在出错时回复，`q>1` 一律不回复。
        match quiet {
            Quiet::Always => response.reply.clear(),
            Quiet::OnError if succeeded => response.reply.clear(),
            _ => {}
        }

        Ok(response)
    }

    fn execute_inner(
        &mut self,
        cmd: &Command,
        cursor: (i32, u32),
    ) -> Result<Response, ExecutionError> {
        match cmd.action {
            Action::Query => {
                // 查询：回报四个 1×1 的占位图像（kitty 约定），
                // 客户端据此判断终端支持哪些格式/动作。必须回显请求的 id，
                // 否则能力探测认不出这是对它的应答。
                Ok(Response {
                    reply: self.ok_reply(cmd.control.u32(b'i')),
                    images_changed: false,
                })
            }
            Action::Transmit => self.transmit(cmd, cursor, false),
            Action::TransmitAndDisplay => self.transmit(cmd, cursor, true),
            Action::Display => self.display(cmd, cursor),
            Action::Delete => self.delete(cmd, cursor),
            Action::TransmitAnimationFrame => self.transmit_animation_frame(cmd),
            Action::ControlAnimation => self.control_animation(cmd),
            Action::ComposeAnimation => self.compose_animation(cmd),
        }
    }

    /// 解析传输参数。
    fn transmission_params(
        &self,
        cmd: &Command,
    ) -> Result<(Medium, Format, Compression, Option<u32>, Option<u32>), ExecutionError> {
        let medium = match cmd.control.byte(b't') {
            None => Medium::Direct,
            Some(b'd') => Medium::Direct,
            Some(b'f') => Medium::File,
            Some(b't') => Medium::TemporaryFile,
            Some(b's') => Medium::SharedMemory,
            Some(_) => return Err(ExecutionError::Parse(ParseError::InvalidFormat)),
        };

        // 介质策略：非直传需要显式开启。
        let medium_allowed = match medium {
            Medium::Direct => true,
            Medium::File => self.policy.allow_file,
            Medium::TemporaryFile => self.policy.allow_temporary_file,
            Medium::SharedMemory => self.policy.allow_shared_memory,
        };
        if !medium_allowed {
            return Err(ExecutionError::UnsupportedMedium);
        }

        let format = match cmd.control.u32(b'f') {
            None => Format::Rgba,
            Some(24) => Format::Rgb,
            Some(0) | Some(32) => Format::Rgba,
            Some(100) => Format::Png,
            Some(_) => return Err(ExecutionError::Decode(DecodeError::UnsupportedFormat)),
        };

        // PNG 负载自带压缩层，`o=z` 与 `f=100` 组合没有意义——按未压缩处理。
        let compression = match cmd.compression {
            Compression::None => Compression::None,
            Compression::Zlib if format == Format::Png => Compression::None,
            Compression::Zlib => Compression::Zlib,
        };

        Ok((
            medium,
            format,
            compression,
            cmd.control.u32(b's'),
            cmd.control.u32(b'v'),
        ))
    }

    /// base64 文本 → 解码 → 按需解压（`o=z`）。
    ///
    /// 分片路径拼完文本、单片路径直接取 `encoded`，之后都要走同一套
    /// "解码 + 解压"顺序：kitty 规定压缩作用于 base64 **解码后**的字节流。
    fn payload_bytes(encoded: &[u8], compression: Compression) -> Result<Vec<u8>, ExecutionError> {
        let decoded = decode_base64(encoded)?;
        match compression {
            Compression::None => Ok(decoded),
            Compression::Zlib => image::decompress_zlib(&decoded).map_err(ExecutionError::Decode),
        }
    }

    /// 按介质取出负载字节。
    ///
    /// `t=f`/`t=t` 的 payload 是**文件路径**而非像素数据（kitty 规范
    /// "local client" 一节）：路径本身仍是 base64 文本，解码后得到路径字符串，
    /// 再从磁盘读取文件内容。ghostty 在
    /// `graphics_image.zig::LoadingImage.readFile` 里实现同一语义。
    ///
    /// `t=t` 额外要求路径落在临时目录内、且文件名含 `tty-graphics-protocol`
    /// （防跨进程误删），读毕即删除该文件——同样对齐 ghostty。
    fn payload_for_medium(
        &self,
        encoded: &[u8],
        medium: Medium,
        compression: Compression,
    ) -> Result<Vec<u8>, ExecutionError> {
        if medium == Medium::Direct {
            return Self::payload_bytes(encoded, compression);
        }

        // 路径本身是 base64 文本；压缩键只作用于文件**内容**。
        let raw_path = decode_base64(encoded)?;
        if raw_path.contains(&0) {
            // POSIX 路径不允许内嵌 NUL。
            return Err(ExecutionError::InvalidData);
        }
        let path = std::str::from_utf8(&raw_path).map_err(|_| ExecutionError::InvalidData)?;
        let fs_path = std::path::Path::new(path);

        // 先打开再校验，避免 TOCTOU（对齐 ghostty "Open our file right away
        // before we do validation"）。
        let file = std::fs::File::open(fs_path).map_err(|_| ExecutionError::InvalidData)?;

        let mut delete_after = false;
        if medium == Medium::TemporaryFile {
            let Some(temp_dir) = self.policy.temporary_directory.as_ref() else {
                return Err(ExecutionError::TemporaryFileNotInTempDir);
            };
            // 用 canonicalize 解析符号链接后再比较，避免借链接逃出临时目录。
            let canonical = fs_path
                .canonicalize()
                .map_err(|_| ExecutionError::InvalidData)?;
            if !canonical.starts_with(temp_dir) {
                return Err(ExecutionError::TemporaryFileNotInTempDir);
            }
            if !canonical
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("tty-graphics-protocol"))
            {
                return Err(ExecutionError::TemporaryFileNotNamedCorrectly);
            }
            delete_after = true;
        }

        // 必须是普通文件（目录 / 设备 / FIFO 一律拒绝）。
        let meta = file.metadata().map_err(|_| ExecutionError::InvalidData)?;
        if !meta.is_file() {
            return Err(ExecutionError::InvalidData);
        }

        let bytes = std::fs::read(fs_path).map_err(|_| ExecutionError::InvalidData)?;

        if delete_after {
            // 删除失败不影响本次读取结果。
            let _ = std::fs::remove_file(fs_path);
        }

        // 压缩作用于文件内容。
        match compression {
            Compression::None => Ok(bytes),
            Compression::Zlib => image::decompress_zlib(&bytes).map_err(ExecutionError::Decode),
        }
    }

    /// 处理传输（`a=t` / `a=T`）。
    fn transmit(
        &mut self,
        cmd: &Command,
        cursor: (i32, u32),
        display: bool,
    ) -> Result<Response, ExecutionError> {
        let (medium, format, compression, width, height) = self.transmission_params(cmd)?;

        // 是否还有后续分片（仅直传介质支持）。
        let more = medium.supports_chunking() && cmd.control.u32(b'm').is_some_and(|v| v > 0);

        // 显式 id：`i=` 为存储键，`I=` 为客户端编号。
        let explicit_id = cmd.control.u32(b'i').filter(|v| *v != 0);
        let number = cmd.control.u32(b'I').filter(|v| *v != 0);

        if more {
            // 分片中间态：拼接 **base64 文本**，等最后一片再整体解码。
            match &mut self.loading {
                Some(loading) if explicit_id.is_none() || Some(loading.id) == explicit_id => {
                    loading.encoded.extend_from_slice(&cmd.encoded);
                    return Ok(Response::default());
                }
                Some(_) => return Err(ExecutionError::InvalidChunk),
                None => {
                    let id = explicit_id
                        .unwrap_or_else(|| self.with_storage_mut(|s| s.allocate_implicit_id()));
                    self.loading = Some(LoadingImage {
                        id,
                        number,
                        format,
                        compression,
                        medium,
                        width,
                        height,
                        encoded: cmd.encoded.clone(),
                        frame: None,
                    });
                    return Ok(Response::default());
                }
            }
        }

        // 末片（或单片）：拼接累积的 base64 文本后解码。
        let (id, payload, format, width, height, number) = match self.loading.take() {
            Some(mut loading) => {
                if let Some(explicit) = explicit_id
                    && explicit != loading.id
                {
                    return Err(ExecutionError::InvalidChunk);
                }
                loading.encoded.extend_from_slice(&cmd.encoded);
                (
                    loading.id,
                    self.payload_for_medium(&loading.encoded, loading.medium, loading.compression)?,
                    loading.format,
                    loading.width,
                    loading.height,
                    loading.number,
                )
            }
            None => {
                let id = explicit_id
                    .unwrap_or_else(|| self.with_storage_mut(|s| s.allocate_implicit_id()));
                (
                    id,
                    self.payload_for_medium(&cmd.encoded, medium, compression)?,
                    format,
                    width,
                    height,
                    number,
                )
            }
        };

        let decoded = image::decode(&payload, format, width, height)?;

        // 首次传输同一 id 时清除旧数据；`transient = !display`。
        let was_transient = !display;
        self.with_storage_mut(|s| s.insert(id, decoded, number, was_transient));

        let mut response = Response {
            reply: self.ok_reply(explicit_id),
            images_changed: true,
        };

        if display {
            self.place_at_cursor(id, cmd, cursor)?;
        } else if explicit_id.is_none() {
            // 隐式 id 必须回报，供客户端后续引用。
            response.reply = self.ok_reply_with_id(id);
        }

        Ok(response)
    }

    /// 处理显示（`a=p`），以及 `a=T` 的放置部分。
    fn display(&mut self, cmd: &Command, cursor: (i32, u32)) -> Result<Response, ExecutionError> {
        let Some(id) = cmd.control.u32(b'i').filter(|v| *v != 0) else {
            return Err(ExecutionError::Parse(ParseError::InvalidFormat));
        };
        if !self.with_storage(|s| s.get(id).is_some()) {
            // 图像不存在：静默成功（kitty 对未知 id 的显示不报错）。
            return Ok(Response::default());
        }
        self.place_at_cursor(id, cmd, cursor)?;
        Ok(Response {
            reply: self.ok_reply(cmd.control.u32(b'i')),
            images_changed: true,
        })
    }

    /// 在光标处建立一个放置。
    fn place_at_cursor(
        &mut self,
        id: ImageId,
        cmd: &Command,
        cursor: (i32, u32),
    ) -> Result<(), ExecutionError> {
        let placement_id = cmd.control.u32(b'p').unwrap_or(0);

        // `X=`/`Y=` 是单元格内偏移，必须落在单个单元格内。
        let x_offset = cmd.control.u32(b'X').unwrap_or(0);
        let y_offset = cmd.control.u32(b'Y').unwrap_or(0);

        let placement = Placement {
            row: cursor.0,
            col: cursor.1,
            columns: cmd.control.u32(b'c').filter(|v| *v != 0),
            rows: cmd.control.u32(b'r').filter(|v| *v != 0),
            x_offset,
            y_offset,
            z: cmd.control.i32(b'z').unwrap_or(0),
            // `U=1` 建虚拟放置：图像不落在光标处，而是等网格里出现 U+10EEEE
            // 占位符时按占位符的位置绘制。行列数在这里只作为"铺进网格多大"
            // 的尺寸说明（见 `unicode` / `render::placeholder_rect`）。
            location: if cmd.control.u32(b'U') == Some(1) {
                PlacementLocation::Virtual
            } else {
                PlacementLocation::Anchor
            },
        };

        if !self.with_storage_mut(|s| s.place(id, placement_id, placement)) {
            return Err(ExecutionError::InvalidChunk);
        }
        Ok(())
    }

    /// 处理动画帧传输（`a=f`）。
    ///
    /// 帧数据的加载方式与图像完全一致（分片、格式、长度校验都相同），区别只在
    /// 于加载完成后不是插入新图像，而是合成进既有图像的动画里。
    ///
    /// 一个特例：**动画的第一帧就是图像本身**（协议的"根帧"），所以当目标图像
    /// 还不存在时，`a=f` 与普通的 `a=t` 无差别——直接复用同一套加载路径。
    fn transmit_animation_frame(&mut self, cmd: &Command) -> Result<Response, ExecutionError> {
        let (medium, format, compression, width, height) = self.transmission_params(cmd)?;
        let edit_frame = cmd.control.u32(b'r').unwrap_or(0);

        // 续片：首片已把帧参数存进 `loading`，这里只贡献负载字节。
        if let Some(loading) = self.loading.as_mut() {
            // 首帧（图像本体）的加载走普通传输逻辑。
            if loading.frame.is_none() {
                return self.transmit(cmd, (0, 0), false);
            }
            loading.encoded.extend_from_slice(&cmd.encoded);
            if medium.supports_chunking() && cmd.control.u32(b'm').is_some_and(|v| v > 0) {
                return Ok(Response::default());
            }
            let done = self.loading.take().expect("上面刚确认存在");
            return self.complete_animation_frame(done);
        }

        // 开始一次新的帧加载（可能是首帧，也可能是后续帧）。
        //
        // 帧必须挂在**既有图像**上，因此 `i=`/`I=` 至少要给一个；且不像普通
        // 传输那样允许隐式分配 id——没有目标图像就无从"挂"起。
        let Some(id) = cmd.control.u32(b'i').filter(|v| *v != 0) else {
            return Err(ExecutionError::Animation("EINVAL: image ID required"));
        };

        let image_generation = match self.with_storage(|s| s.get(id).map(|st| st.generation)) {
            Some(generation) => generation,
            None => {
                // 图像不存在。这只有在 `r` 未指定或指定为 1（即"这是根帧"）
                // 时才合法——此时按普通传输创建图像。其余帧号无图像可依附。
                if edit_frame == 0 || edit_frame == 1 {
                    return self.transmit(cmd, (0, 0), false);
                }
                return Err(ExecutionError::Animation("ENOENT: image not found"));
            }
        };

        let frame = FrameLoad {
            image_generation,
            edit_frame,
            create_frame: cmd.control.u32(b'c').unwrap_or(0),
            gap_ms: cmd.control.i32(b'z').unwrap_or(0),
            x: cmd.control.u32(b'x').unwrap_or(0),
            y: cmd.control.u32(b'y').unwrap_or(0),
            // `X=1` 覆盖，其余混合（与 ghostty 一致，只测是否为 1）。
            composition_mode: if cmd.control.u32(b'X') == Some(1) {
                CompositionMode::Overwrite
            } else {
                CompositionMode::AlphaBlend
            },
            background: Background::from_bits(cmd.control.u32(b'Y').unwrap_or(0)),
        };

        let more = medium.supports_chunking() && cmd.control.u32(b'm').is_some_and(|v| v > 0);
        self.loading = Some(LoadingImage {
            id,
            number: cmd.control.u32(b'I').filter(|v| *v != 0),
            format,
            compression,
            medium,
            width,
            height,
            encoded: cmd.encoded.clone(),
            frame: Some(frame),
        });

        if more {
            // 中间态：等待剩余分片，暂不应答。
            return Ok(Response::default());
        }

        let done = self.loading.take().expect("上面刚设置");
        self.complete_animation_frame(done)
    }

    /// 完成一个已完整加载的动画帧：校验、编号、合成。
    fn complete_animation_frame(
        &mut self,
        mut loading: LoadingImage,
    ) -> Result<Response, ExecutionError> {
        let spec = loading.frame.take().expect("只有帧加载才会走到这里");

        // 按保存的世代号重新解析图像：加载期间它可能已被删除、淘汰或替换。
        let (exists, generation, image_width, image_height, frame_count) =
            self.with_storage(|s| match s.get(loading.id) {
                Some(stored) => (
                    true,
                    stored.generation,
                    stored.image.width,
                    stored.image.height,
                    stored.animation.as_ref().map_or(1, |a| a.frame_count()),
                ),
                None => (false, 0, 0, 0, 0),
            });
        if !exists || generation != spec.image_generation {
            return Err(ExecutionError::Animation("ENOENT: image not found"));
        }

        // 帧数据解码：校验规则与图像加载完全相同。
        let payload = Self::payload_bytes(&loading.encoded, loading.compression)?;
        let frame_img = image::decode(&payload, loading.format, loading.width, loading.height)?;

        // 帧矩形不得超过图像尺寸（`x`/`y` 偏移故意不校验：合成会裁剪，对齐 kitty）。
        if frame_img.width > image_width || frame_img.height > image_height {
            return Err(ExecutionError::Animation(
                "EINVAL: frame dimensions exceed image",
            ));
        }

        // 解析目标帧号：`r` 落在 `1..=count` 时编辑该帧，否则新建并追加在
        // `count + 1`。解析出的号要回报给客户端，它才知道新建帧的编号。
        let number = {
            let r = spec.edit_frame;
            if r == 0 || r > frame_count + 1 {
                frame_count + 1
            } else {
                r
            }
        };

        if number == frame_count + 1 {
            self.create_animation_frame(loading.id, number, &spec, &frame_img)
        } else {
            self.edit_animation_frame(loading.id, number, &spec, &frame_img)
        }
    }

    /// 新建一个动画帧（`a=f` 且帧号落在末尾之后）。
    fn create_animation_frame(
        &mut self,
        id: ImageId,
        number: u32,
        spec: &FrameLoad,
        frame_img: &image::DecodedImage,
    ) -> Result<Response, ExecutionError> {
        // 新帧的间隔：正数直接用，负数表示无间隔（永不显示），0 用默认值。
        let gap: u32 = if spec.gap_ms > 0 {
            spec.gap_ms as u32
        } else if spec.gap_ms < 0 {
            0
        } else {
            DEFAULT_GAP_MS
        };

        // 先做全部校验并取到合成所需的参数，避免在"预留空间"（会淘汰其他
        // 图像）之后再回头做检查。
        let prep = self.with_storage(|s| {
            let stored = s.get(id)?;
            // 底画帧必须存在——在预留空间之前就校验，避免白折腾淘汰。
            if spec.create_frame > 0 && stored.frame_data(spec.create_frame).is_none() {
                return None;
            }
            Some((stored.image.width, stored.image.height))
        });
        let Some((image_width, image_height)) = prep else {
            return Err(ExecutionError::Animation("EINVAL: base frame not found"));
        };
        let canvas_len = image_width as usize * image_height as usize * 4;

        // 先预留空间（必要时淘汰**其他**图像），再真正分配。淘汰只可能移除
        // 别的图像，绝不会动受益图像本身。
        if !self.with_storage_mut(|s| s.reserve_animation_bytes(id, canvas_len)) {
            return Err(ExecutionError::Animation(
                "ENOSPC: animation frame storage full",
            ));
        }

        // 底画：给定 `c=` 时复制该帧，否则用背景色填充。
        let base_frame = self.with_storage(|s| {
            if spec.create_frame == 0 {
                return None;
            }
            s.get(id)
                .and_then(|stored| stored.frame_data(spec.create_frame))
                .map(|data| data.to_vec())
        });
        let mut canvas = base_frame.unwrap_or_else(|| {
            let mut c = vec![0u8; canvas_len];
            animation::fill_background(&mut c, spec.background);
            c
        });

        animation::compose_rect(
            &mut canvas,
            image_width,
            image_height,
            &frame_img.rgba,
            frame_img.width,
            frame_img.height,
            spec.x,
            spec.y,
            spec.composition_mode,
        );

        self.with_storage_mut(|s| {
            let stored = s.get_mut(id).expect("受益图像不应被淘汰");
            let anim = stored.animation.get_or_insert_with(Animation::default);
            anim.frames.push(Frame {
                data: canvas,
                gap_ms: gap,
            });
            // 新建帧不会改变当前显示帧（播放以后才会走到它），但存储内容变了，
            // 需要让上层知道有变化以触发重绘。
        });

        Ok(Response {
            reply: self.animation_ok_reply(id, number),
            images_changed: true,
        })
    }

    /// 编辑一个既有动画帧（`a=f` 且 `r` 命中既有帧号）。
    fn edit_animation_frame(
        &mut self,
        id: ImageId,
        number: u32,
        spec: &FrameLoad,
        frame_img: &image::DecodedImage,
    ) -> Result<Response, ExecutionError> {
        // 编辑没有字节预留这一步：帧恒为整幅大小，编辑就是就地合成，
        // 存储占用不变。kitty 同样豁免帧编辑的配额检查。
        let is_current = self.with_storage_mut(|s| {
            let Some(stored) = s.get_mut(id) else {
                return false;
            };
            let (width, height) = (stored.image.width, stored.image.height);
            let Some(anim) = stored.animation.as_mut() else {
                return false;
            };

            // 非 0 的间隔才更新；编辑不套用 40ms 默认值。
            if spec.gap_ms != 0 {
                anim.set_gap_at(
                    number - 1,
                    if spec.gap_ms > 0 {
                        spec.gap_ms as u32
                    } else {
                        0
                    },
                );
            }

            let is_current = number - 1 == anim.current_index;
            if let Some(index) = number.checked_sub(2).map(|i| i as usize)
                && let Some(frame) = anim.frames.get_mut(index)
            {
                animation::compose_rect(
                    &mut frame.data,
                    width,
                    height,
                    &frame_img.rgba,
                    frame_img.width,
                    frame_img.height,
                    spec.x,
                    spec.y,
                    spec.composition_mode,
                );
            }

            if is_current {
                // 显示中的像素变了：重置间隔计时，让当前帧的计时重新开始。
                anim.frame_shown_at_ms = None;
            }
            is_current
        });

        // 显示中的像素变了 → 换发新世代，让渲染层的纹理缓存失效。
        if is_current {
            self.with_storage_mut(|s| s.mark_content_changed(id));
        }

        Ok(Response {
            reply: self.animation_ok_reply(id, number),
            images_changed: true,
        })
    }

    /// 处理动画播放控制（`a=a`）。
    ///
    /// 命令成功时**不应答**（对齐 kitty）；只有图像/标识符缺失才回错误。
    /// 非法的键值被静默忽略。
    fn control_animation(&mut self, cmd: &Command) -> Result<Response, ExecutionError> {
        let id = cmd.control.u32(b'i').filter(|v| *v != 0);
        let number = cmd.control.u32(b'I').filter(|v| *v != 0);
        if id.is_none() && number.is_none() {
            return Err(ExecutionError::Animation(
                "EINVAL: image ID or number required",
            ));
        }
        let Some(id) = self.resolve_image(id, number) else {
            return Err(ExecutionError::Animation("ENOENT: image not found"));
        };

        let frame = cmd.control.u32(b'r').unwrap_or(0);
        let gap_ms = cmd.control.i32(b'z').unwrap_or(0);
        let current_frame = cmd.control.u32(b'c').unwrap_or(0);
        let loops = cmd.control.u32(b'v').unwrap_or(0);
        let action = match cmd.control.u32(b's') {
            Some(1) => Some(State::Stopped),
            Some(2) => Some(State::Loading),
            Some(3) => Some(State::Running),
            // kitty 忽略不认识的状态值，保持动画状态不变。
            _ => None,
        };

        let mut content_changed = false;
        self.with_storage_mut(|s| {
            let Some(stored) = s.get_mut(id) else {
                return;
            };
            let anim = stored.animation.get_or_insert_with(Animation::default);
            let count = anim.frame_count();

            // 以下键**各自独立**生效，顺序与 kitty 一致。

            // 设置某一帧的间隔（`r=` 配 `z=`）。这是给根帧设间隔的唯一途径，
            // 因为根帧创建时是无间隔的。
            if frame != 0 && frame <= count && gap_ms != 0 {
                anim.set_gap_at(frame - 1, if gap_ms > 0 { gap_ms as u32 } else { 0 });
            }

            // 设置当前帧（`c=`），客户端驱动动画的基本手段。
            if current_frame != 0
                && current_frame <= count
                && current_frame - 1 != anim.current_index
            {
                anim.current_index = current_frame - 1;
                anim.frame_shown_at_ms = None;
                content_changed = true;
            }

            // 设置播放状态（`s=`）。任何状态变化都重置循环计数；离开 stopped
            // 会重启间隔计时。
            if let Some(state) = action {
                let old = anim.state;
                anim.state = state;
                if old == State::Stopped && state != State::Stopped {
                    anim.frame_shown_at_ms = None;
                }
                anim.current_loop = 0;
            }

            // 设置循环轮数（`v=`）。按协议差一存储：`v=1` 无限循环，
            // `v=n` 播 n-1 轮。
            if loops != 0 {
                anim.max_loops = loops - 1;
            }
        });

        if content_changed {
            self.with_storage_mut(|s| s.mark_content_changed(id));
        }

        // 成功的动画控制命令不应答。
        Ok(Response {
            reply: Vec::new(),
            images_changed: true,
        })
    }

    /// 处理帧间合成（`a=c`）。
    ///
    /// 注意 `r=` 是**源**帧、`c=` 是**目标**帧（规范里的表格把两者写反了）。
    /// 帧 1（根帧）恒存在，所以对没有动画状态的图像也能用。
    fn compose_animation(&mut self, cmd: &Command) -> Result<Response, ExecutionError> {
        let id = cmd.control.u32(b'i').filter(|v| *v != 0);
        let number = cmd.control.u32(b'I').filter(|v| *v != 0);
        if id.is_none() && number.is_none() {
            return Err(ExecutionError::Animation(
                "EINVAL: image ID or number required",
            ));
        }
        let Some(id) = self.resolve_image(id, number) else {
            return Err(ExecutionError::Animation("ENOENT: image not found"));
        };

        let source_frame = cmd.control.u32(b'r').unwrap_or(0);
        let dest_frame = cmd.control.u32(b'c').unwrap_or(0);
        let x = cmd.control.u32(b'x').unwrap_or(0);
        let y = cmd.control.u32(b'y').unwrap_or(0);
        let left_edge = cmd.control.u32(b'X').unwrap_or(0);
        let top_edge = cmd.control.u32(b'Y').unwrap_or(0);
        let width = cmd.control.u32(b'w').unwrap_or(0);
        let height = cmd.control.u32(b'h').unwrap_or(0);
        // kitty 对 compose 是"非 0 即覆盖"。
        let mode = if cmd.control.u32(b'C') == Some(0) {
            CompositionMode::AlphaBlend
        } else {
            CompositionMode::Overwrite
        };

        // 先只读地把源帧数据与图像尺寸取出来（取副本，避免同时持有源与目标的
        // 借用），并在此完成"帧是否存在"的校验。
        let geometry = self.with_storage(|s| {
            let stored = s.get(id)?;
            // 源帧与目标帧都必须存在。注意 `r` 是源、`c` 是目标。
            let src = stored.frame_data(source_frame)?.to_vec();
            stored.frame_data(dest_frame)?;
            Some((stored.image.width, stored.image.height, src))
        });
        let Some((img_width, img_height, src)) = geometry else {
            return Err(ExecutionError::Animation("ENOENT: frame not found"));
        };

        // 矩形校验用 u64，防止不可信的 32 位值溢出。越界在这里是**错误**，
        // 与 `a=f`（裁剪）不同。
        let w: u64 = if width > 0 {
            width as u64
        } else {
            img_width as u64
        };
        let h: u64 = if height > 0 {
            height as u64
        } else {
            img_height as u64
        };
        if x as u64 + w > img_width as u64 || y as u64 + h > img_height as u64 {
            return Err(ExecutionError::Animation(
                "EINVAL: destination rectangle out of bounds",
            ));
        }
        if left_edge as u64 + w > img_width as u64 || top_edge as u64 + h > img_height as u64 {
            return Err(ExecutionError::Animation(
                "EINVAL: source rectangle out of bounds",
            ));
        }

        // 把帧合成到自身时，两个矩形必须不相交。
        if source_frame == dest_frame {
            let x_overlaps = left_edge.max(x) < left_edge.min(x) + w as u32;
            let y_overlaps = top_edge.max(y) < top_edge.min(y) + h as u32;
            if x_overlaps && y_overlaps {
                return Err(ExecutionError::Animation(
                    "EINVAL: source and destination rectangles overlap",
                ));
            }
        }

        // 目标帧可能是根帧（图像自身数据），也可能在动画里；两者要写回不同位置。
        let dest_is_root = dest_frame == 1;
        let dest_index = dest_frame.checked_sub(2).map(|i| i as usize);

        self.with_storage_mut(|s| {
            let Some(stored) = s.get_mut(id) else {
                return;
            };
            if dest_is_root {
                animation::compose_canvas_rect(
                    &mut stored.image.rgba,
                    &src,
                    img_width,
                    w as u32,
                    h as u32,
                    left_edge,
                    top_edge,
                    x,
                    y,
                    mode,
                );
            } else if let Some(index) = dest_index {
                let anim = match stored.animation.as_mut() {
                    Some(anim) => anim,
                    None => return,
                };
                if let Some(frame) = anim.frames.get_mut(index) {
                    animation::compose_canvas_rect(
                        &mut frame.data,
                        &src,
                        img_width,
                        w as u32,
                        h as u32,
                        left_edge,
                        top_edge,
                        x,
                        y,
                        mode,
                    );
                }
            }
        });

        // 目标是显示中的帧 → 屏幕内容变了；否则只是存储内容变了。
        let current =
            self.with_storage(|s| s.get(id).map_or(0, |stored| stored.current_frame_index()));
        if dest_frame == current + 1 {
            self.with_storage_mut(|s| s.mark_content_changed(id));
        }

        Ok(Response {
            reply: Vec::new(),
            images_changed: true,
        })
    }

    /// 按 `(i=, I=)` 解析出图像 id。
    fn resolve_image(&self, id: Option<u32>, number: Option<u32>) -> Option<ImageId> {
        if let Some(id) = id {
            return self.with_storage(|s| s.get(id).map(|_| id));
        }
        let number = number?;
        self.with_storage(|s| s.newest_by_number(number))
    }

    /// 动画帧操作的成功应答：带图像 id 与解析出的帧号（`r=`）。
    ///
    /// 帧号是客户端学习"新建帧被分配了什么编号"的唯一途径。
    fn animation_ok_reply(&self, id: ImageId, frame: u32) -> Vec<u8> {
        format!("\x1b_Gi={id},r={frame};OK\x1b\\").into_bytes()
    }

    /// 处理删除（`a=d`）。
    fn delete(&mut self, cmd: &Command, cursor: (i32, u32)) -> Result<Response, ExecutionError> {
        let Some(delete_byte) = cmd.control.byte(b'd') else {
            return Err(ExecutionError::Parse(ParseError::InvalidFormat));
        };
        let Some((target, free_data)) = DeleteTarget::from_byte(delete_byte) else {
            return Err(ExecutionError::Parse(ParseError::InvalidFormat));
        };

        let changed = match target {
            DeleteTarget::All => self.with_storage_mut(|s| {
                let had = !s.is_empty();
                s.clear();
                had
            }),
            DeleteTarget::ByImageId => {
                let id = cmd.control.u32(b'i').unwrap_or(0);
                let placement_id = cmd.control.u32(b'p');
                match placement_id {
                    // `p=0` 或缺省：删除该图像的全部放置。
                    None | Some(0) => self.with_storage_mut(|s| {
                        if free_data {
                            s.remove(id).is_some()
                        } else if let Some(stored) = s.get_mut(id) {
                            let had = !stored.placements.is_empty();
                            stored.placements.clear();
                            had
                        } else {
                            false
                        }
                    }),
                    // 指定放置：只删那一个；图像数据保留。
                    Some(p) => self.with_storage_mut(|s| {
                        let removed = s.remove_placement(id, p);
                        if free_data {
                            // 大写时若已无放置，则连数据一并释放。
                            let unused =
                                s.get(id).is_some_and(|stored| stored.placements.is_empty());
                            if unused {
                                s.remove(id);
                            }
                        }
                        removed
                    }),
                }
            }
            DeleteTarget::ByImageNumber => {
                let number = cmd.control.u32(b'I').unwrap_or(0);
                self.with_storage_mut(|s| {
                    let Some(id) = s.newest_by_number(number) else {
                        return false;
                    };
                    if free_data {
                        s.remove(id).is_some()
                    } else if let Some(stored) = s.get_mut(id) {
                        let had = !stored.placements.is_empty();
                        stored.placements.clear();
                        had
                    } else {
                        false
                    }
                })
            }
            DeleteTarget::AtCursor => self.delete_intersecting(cursor.0, cursor.1, free_data),
            DeleteTarget::AtCell | DeleteTarget::AtCellWithZ => {
                // `x=`/`y=` 为 1-based 单元格坐标。
                let x = cmd.control.u32(b'x').unwrap_or(1).saturating_sub(1);
                let y = cmd.control.u32(b'y').unwrap_or(1).saturating_sub(1);
                self.delete_intersecting(y as i32, x, free_data)
            }
            DeleteTarget::ByColumn => {
                let x = cmd.control.u32(b'x').unwrap_or(1).saturating_sub(1);
                self.delete_matching(|p| p.col == x, free_data)
            }
            DeleteTarget::ByRow => {
                let y = cmd.control.u32(b'y').unwrap_or(1).saturating_sub(1);
                self.delete_matching(|p| p.row == y as i32, free_data)
            }
            DeleteTarget::ByImageIdRange => {
                let lo = cmd.control.u32(b'x').unwrap_or(0);
                let hi = cmd.control.u32(b'y').unwrap_or(0);
                self.delete_matching_image(|id| id >= lo && id <= hi, free_data)
            }
            DeleteTarget::ByZIndex => {
                let z = cmd.control.i32(b'z').unwrap_or(0);
                self.delete_matching(|p| p.z == z, free_data)
            }
            DeleteTarget::AnimationFrames => {
                // `d=f`/`d=F`：删除动画帧。帧号由 `r=` 给出（0/缺省表示根帧）。
                let id = cmd.control.u32(b'i').filter(|v| *v != 0);
                let number = cmd.control.u32(b'I').filter(|v| *v != 0);
                let frame = cmd.control.u32(b'r').unwrap_or(0);
                match self.resolve_image(id, number) {
                    Some(id) => {
                        self.with_storage_mut(|s| s.delete_animation_frame(id, frame, free_data))
                    }
                    None => false,
                }
            }
        };

        Ok(Response {
            reply: self.ok_reply(cmd.control.u32(b'i')),
            images_changed: changed,
        })
    }

    /// 删除与指定单元格相交的放置。
    fn delete_intersecting(&mut self, row: i32, col: u32, free_data: bool) -> bool {
        self.delete_matching(|p| p.row == row && p.col == col, free_data)
    }

    /// 按谓词删除放置；`free_data` 为真时释放不再被引用的图像。
    fn delete_matching(
        &mut self,
        mut pred: impl FnMut(&Placement) -> bool,
        free_data: bool,
    ) -> bool {
        // 先收集待删项，避免在持有存储不可变借用时又要可变借用。
        let mut to_remove: Vec<(ImageId, u32)> = Vec::new();
        self.with_storage(|s| {
            for (id, stored) in s.iter() {
                for (pid, p) in stored.placements.iter() {
                    if pred(p) {
                        to_remove.push((*id, *pid));
                    }
                }
            }
        });

        if to_remove.is_empty() {
            return false;
        }

        self.with_storage_mut(|s| {
            let mut emptied: Vec<ImageId> = Vec::new();
            for (id, pid) in to_remove {
                s.remove_placement(id, pid);
                if free_data
                    && s.get(id).is_some_and(|stored| stored.placements.is_empty())
                    && !emptied.contains(&id)
                {
                    emptied.push(id);
                }
            }

            for id in emptied {
                s.remove(id);
            }
            true
        })
    }

    /// 按图像 id 谓词删除。
    fn delete_matching_image(
        &mut self,
        mut pred: impl FnMut(ImageId) -> bool,
        free_data: bool,
    ) -> bool {
        self.with_storage_mut(|s| {
            let victims: Vec<ImageId> =
                s.iter().map(|(id, _)| *id).filter(|id| pred(*id)).collect();
            let changed = !victims.is_empty();
            for id in victims {
                if free_data {
                    s.remove(id);
                } else if let Some(stored) = s.get_mut(id) {
                    stored.placements.clear();
                }
            }
            changed
        })
    }

    /// 成功响应（`OK`）。
    ///
    /// `request_id` 是请求里 `i=` 携带的图像 id：kitty 规定响应必须**原样回显
    /// 该 id**，客户端靠它把响应和请求对上。能力探测就是靠这个握手的——
    /// 例如 terminal-browser 发 `i=4207,a=q,...` 后只认包含 `Gi=4207;` 的回复，
    /// 回 `i=0` 会被判成"终端不支持图像"。请求没带 id 时才回 `i=0`。
    fn ok_reply(&self, request_id: Option<u32>) -> Vec<u8> {
        match request_id {
            Some(id) => self.ok_reply_with_id(id),
            None => b"\x1b_Gi=0;OK\x1b\\".to_vec(),
        }
    }

    /// 带图像 id 的成功响应（隐式 id 时必须回报）。
    fn ok_reply_with_id(&self, id: ImageId) -> Vec<u8> {
        format!("\x1b_Gi={id};OK\x1b\\").into_bytes()
    }

    /// 错误响应。
    fn error_reply(&self, message: &str) -> Vec<u8> {
        format!("\x1b_G;{message}\x1b\\").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::kitty::command::Parser;

    fn run(
        store: &mut ImageStore,
        seq: &str,
        cursor: (i32, u32),
    ) -> Result<Response, ExecutionError> {
        let mut p = Parser::new();
        p.feed_slice(seq.as_bytes()).expect("解析失败");
        let cmd = p.finish().expect("命令不完整");
        store.execute(cmd, cursor)
    }

    fn rgba_pixel() -> &'static str {
        // 1×1 红色 RGBA，base64 编码。
        "/wAA/w=="
    }

    /// 生成 `o=z` 的测试负载：把 `raw` zlib 压缩后 base64 编码。
    fn zlib_b64(raw: &[u8]) -> String {
        use std::io::Write as _;

        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(raw).expect("压缩失败");
        let compressed = enc.finish().expect("压缩收尾失败");
        base64_encode(&compressed)
    }

    /// 标准字母表 base64 编码（kitty 用它承载负载与路径）。
    fn base64_encode(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut b64 = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            b64.push(TABLE[(n >> 18) as usize & 63] as char);
            b64.push(TABLE[(n >> 12) as usize & 63] as char);
            b64.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            b64.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        b64
    }

    /// 在临时目录里写一个 `t=t`/`t=f` 用的载荷文件。
    ///
    /// 文件名含 `tty-graphics-protocol`——这是 `t=t` 的协议要求（ghostty
    /// `readFile` 里 `TemporaryFileNotNamedCorrectly` 的判据）。
    fn write_temp_payload(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tty-graphics-protocol-{tag}-{}.bin",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("写入测试载荷失败");
        path.canonicalize().expect("canonicalize 失败")
    }

    #[test]
    fn zlib_single_chunk_transmits_and_displays() {
        // terminal-browser 的传帧形态：a=T,f=32,o=z,s=..,v=..（单片、压缩）。
        let mut store = ImageStore::new(1024 * 1024);
        // 2×2 RGBA = 16 字节。
        let raw: Vec<u8> = (0..16).map(|i| i as u8).collect();
        let payload = zlib_b64(&raw);
        let r = run(
            &mut store,
            &format!("a=T,i=3,f=32,o=z,s=2,v=2;{payload}"),
            (0, 0),
        )
        .unwrap();
        assert!(r.images_changed);
        let guard = store.lock_storage();
        let stored = guard.get(3).expect("压缩图像应入存储");
        assert_eq!(stored.image.width, 2);
        assert_eq!(stored.image.height, 2);
        assert_eq!(stored.image.rgba, raw, "解压后应还原原始像素");
    }

    #[test]
    fn zlib_chunked_transmission_keeps_compression_from_first_chunk() {
        // 首片带 o=z、末片只带 m=0：压缩信息必须由 LoadingImage 保存。
        let mut store = ImageStore::new(1024 * 1024);
        let raw = vec![7u8, 8, 9, 10];
        let payload = zlib_b64(&raw);
        let (a, b) = payload.split_at(payload.len() / 2);
        run(
            &mut store,
            &format!("a=t,i=5,f=32,o=z,s=1,v=1,m=1;{a}"),
            (0, 0),
        )
        .unwrap();
        let r = run(&mut store, &format!("a=t,i=5,f=32,s=1,v=1,m=0;{b}"), (0, 0)).unwrap();
        assert!(r.images_changed);
        let guard = store.lock_storage();
        let stored = guard.get(5).expect("分片压缩图像应入存储");
        assert_eq!(stored.image.rgba, raw);
    }

    #[test]
    fn zlib_invalid_value_is_rejected_at_parse() {
        // o=q 非法：解析阶段即报 InvalidFormat（对齐 ghostty）。
        let mut p = Parser::new();
        p.feed_slice(b"a=t,i=1,f=32,o=q,s=1,v=1;AAAA").unwrap();
        let err = p.finish().unwrap_err();
        assert!(matches!(
            err,
            crate::terminal::kitty::command::ParseError::InvalidFormat
        ));
    }

    #[test]
    fn zlib_corrupt_stream_reports_decompression_error() {
        let mut store = ImageStore::new(1024 * 1024);
        // "this is not zlib" 的 base64：解压失败转为错误应答回给客户端。
        let r = run(
            &mut store,
            "a=t,i=1,f=32,o=z,s=1,v=1;dGhpcyBpcyBub3QgemxpYg==",
            (0, 0),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&r.reply).contains("decompression failed"));
        assert!(!r.images_changed);
        assert!(store.lock_storage().get(1).is_none(), "坏流不得入存储");
    }

    #[test]
    fn transmit_then_display_creates_placement() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(
            &mut store,
            &format!("a=t,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert!(r.images_changed);
        assert!(store.lock_storage().get(1).is_some());

        let r = run(&mut store, "a=p,i=1", (3, 5)).unwrap();
        assert!(r.images_changed);
        let guard = store.lock_storage();

        let stored = guard.get(1).unwrap();
        assert_eq!(stored.placements.len(), 1);
        let placement = stored.placements.get(&0).unwrap();
        assert_eq!((placement.row, placement.col), (3, 5));
    }

    #[test]
    fn transmit_and_display_in_one_command() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=T,i=2,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (1, 1),
        )
        .unwrap();
        let guard = store.lock_storage();

        let stored = guard.get(2).unwrap();
        assert_eq!(stored.placements.len(), 1, "a=T 应同时建立放置");
        assert!(!stored.transient, "a=T 不是临时图像");
    }

    #[test]
    fn chunked_transmission_assembles_payload() {
        let mut store = ImageStore::new(1024 * 1024);
        // 分两片传输同一张 1×1 图：前半（m=1）+ 后半（m=0）。
        let full = rgba_pixel(); // "/wAA/w=="
        let (a, b) = full.split_at(4);

        let r = run(&mut store, &format!("a=t,i=9,f=32,s=1,v=1,m=1;{a}"), (0, 0)).unwrap();
        assert!(!r.images_changed, "分片中间态不应产生图像");

        let r = run(&mut store, &format!("a=t,i=9,f=32,s=1,v=1,m=0;{b}"), (0, 0)).unwrap();
        assert!(r.images_changed);

        let guard = store.lock_storage();

        let stored = guard.get(9).unwrap();
        assert_eq!(
            stored.image.rgba,
            vec![255, 0, 0, 255],
            "分片应正确拼接解码"
        );
    }

    #[test]
    fn delete_all_clears_storage() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=T,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert!(!store.lock_storage().is_empty());

        let r = run(&mut store, "a=d,d=A", (0, 0)).unwrap();
        assert!(r.images_changed);
        assert!(store.lock_storage().is_empty(), "d=A 应清空全部图像");
    }

    #[test]
    fn lowercase_delete_keeps_image_data() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=T,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();

        run(&mut store, "a=d,d=i,i=1", (0, 0)).unwrap();
        let guard = store.lock_storage();

        let stored = guard.get(1).unwrap();
        assert!(stored.placements.is_empty(), "小写 d=i 应删除放置");
        assert_eq!(stored.image.rgba.len(), 4, "小写 d=i 应保留图像数据");
    }

    #[test]
    fn uppercase_by_id_frees_data() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=T,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        run(&mut store, "a=d,d=I,i=1", (0, 0)).unwrap();
        assert!(
            store.lock_storage().get(1).is_none(),
            "大写 d=I 应释放图像数据"
        );
    }

    #[test]
    fn display_unknown_id_is_silent() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "a=p,i=999", (0, 0)).unwrap();
        assert!(!r.images_changed);
        assert!(r.reply.is_empty(), "未知 id 的显示不放置也不回复");
    }

    #[test]
    fn quiet_suppresses_all_replies() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "a=q,q=2", (0, 0)).unwrap();
        assert!(r.reply.is_empty(), "q=2 应完全静默");
    }

    #[test]
    fn quiet_on_error_suppresses_success_reply() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "a=q,q=1", (0, 0)).unwrap();
        assert!(r.reply.is_empty(), "q=1 成功时不回复");
    }

    #[test]
    fn error_is_reported_in_reply_not_as_rust_error() {
        let mut store = ImageStore::new(1024 * 1024);
        // 负载长度与声明的 1×1 RGBA 不符 → 解码失败。
        // 注意：协议错误按 kitty 约定通过**响应文本**回给客户端，
        // 而不是以 Rust 错误返回，因此这里断言的是 reply 内容。
        let r = run(&mut store, "a=t,i=1,f=32,s=1,v=1;AAAA", (0, 0)).unwrap();
        let reply = String::from_utf8_lossy(&r.reply);
        assert!(reply.contains("EINVAL"), "应回报 EINVAL: {reply}");
        assert!(!r.images_changed);
        assert!(
            store.lock_storage().get(1).is_none(),
            "解码失败不应写入图像"
        );
    }

    #[test]
    fn disabled_store_ignores_commands() {
        let mut store = ImageStore::new(0);
        assert!(!store.enabled());
        let r = run(
            &mut store,
            &format!("a=T,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert!(!r.images_changed);
        assert!(store.lock_storage().is_empty());
    }

    #[test]
    fn file_medium_reads_path_payload_and_displays() {
        // terminal-browser 的真实命令形态：
        //   a=T,f=32,s=4,v=4,t=f,i=1,C=1,q=2;<路径的 base64>
        // payload 是**路径**而不是像素；`q=2` 下不应答，只能靠图像是否入库判断。
        // 4×4 RGBA = 64 字节，与声明的 s=v=4 一致。
        let mut raw = Vec::new();
        for _ in 0..16 {
            raw.extend_from_slice(&[0, 255, 0, 255]); // 绿色 RGBA
        }
        let path = write_temp_payload("wide", &raw);

        let mut store = ImageStore::new(1024 * 1024);
        let seq = format!(
            "a=T,f=32,s=4,v=4,t=f,i=1,q=2;{b64}",
            b64 = base64_encode(path.to_str().unwrap().as_bytes())
        );
        let r = run(&mut store, &seq, (0, 0)).unwrap();

        assert!(
            r.images_changed,
            "t=f 应从文件读出像素并入库（这正是 terminal-browser 的路径）"
        );
        assert!(r.reply.is_empty(), "q=2 应静默");

        let guard = store.lock_storage();
        let stored = guard.get(1).expect("图像 1 应存在");
        assert_eq!((stored.image.width, stored.image.height), (4, 4));
        assert_eq!(
            &stored.image.rgba[..4],
            &[0, 255, 0, 255],
            "应解出文件里的绿色像素"
        );
        assert_eq!(stored.placements.len(), 1, "a=T 应同时建立放置");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_medium_missing_file_is_invalid_data() {
        let mut store = ImageStore::new(1024 * 1024);
        let seq = format!(
            "a=t,f=100,i=1,t=f;{b64}",
            b64 = base64_encode(b"/nonexistent/definitely/not/here.png")
        );
        let r = run(&mut store, &seq, (0, 0)).unwrap();
        let reply = String::from_utf8_lossy(&r.reply);
        assert!(
            reply.contains("EINVAL: invalid data"),
            "不存在的路径应回报 EINVAL: invalid data，实际：{reply}"
        );
    }

    #[test]
    fn temporary_file_medium_is_deleted_after_read() {
        // `t=t` 的文件必须位于临时目录且文件名含 tty-graphics-protocol，
        // 读完即删除（对齐 ghostty readFile 的 delete_path 逻辑）。
        let raw = [255u8, 0, 255, 255];
        let path = write_temp_payload("tt", &raw);

        let mut store = ImageStore::new(1024 * 1024);
        let seq = format!(
            "a=t,f=32,s=1,v=1,t=t,i=9;{b64}",
            b64 = base64_encode(path.to_str().unwrap().as_bytes())
        );
        let r = run(&mut store, &seq, (0, 0)).unwrap();
        assert!(r.images_changed, "t=t 应读出数据");
        assert!(!path.exists(), "t=t 读取后应删除临时文件（防残留）");
    }

    #[test]
    fn temporary_file_outside_temp_dir_is_refused() {
        // 路径不在临时目录内 → 拒绝，且**不删除**该文件。
        let raw = [1u8, 2, 3, 255];
        let dir = std::env::temp_dir().join("dt-kitty-nontmp");
        let _ = std::fs::create_dir_all(&dir);
        // 故意让文件名不含 tty-graphics-protocol，且路径经符号链接绕过前缀比较
        // 也无法通过 canonicalize 检查。
        let path = dir.join("plain-file");
        std::fs::write(&path, raw).unwrap();

        let mut store = ImageStore::new(1024 * 1024);
        let seq = format!(
            "a=t,f=32,s=1,v=1,t=t,i=9;{b64}",
            b64 = base64_encode(path.to_str().unwrap().as_bytes())
        );
        let r = run(&mut store, &seq, (0, 0)).unwrap();
        let reply = String::from_utf8_lossy(&r.reply);
        assert!(
            reply.contains("temporary file not"),
            "非规范临时文件应被拒绝，实际：{reply}"
        );
        assert!(path.exists(), "被拒绝的文件绝不能被删除");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn direct_policy_refuses_file_medium() {
        // 收紧策略时仍能挡住文件介质（安全开关必须有效）。
        let mut store = ImageStore::new(1024 * 1024);
        store.set_policy(MediumPolicy::direct());
        let r = run(&mut store, "a=t,t=f,f=100,i=1", (0, 0)).unwrap();
        let reply = String::from_utf8_lossy(&r.reply);
        assert!(
            reply.contains("ENOTSUP"),
            "direct 策略应回报不支持: {reply}"
        );
        assert!(!r.images_changed);
    }

    #[test]
    fn z_index_round_trips() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=T,i=1,f=32,s=1,v=1,z=-1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        let guard = store.lock_storage();

        let stored = guard.get(1).unwrap();
        let placement = stored.placements.get(&0).unwrap();
        assert_eq!(placement.z, -1);
        assert!(placement.is_below_text());
    }

    #[test]
    fn implicit_id_is_reported_back() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(
            &mut store,
            &format!("a=t,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        let reply = String::from_utf8_lossy(&r.reply);
        assert!(reply.contains("i="), "隐式 id 必须回报: {reply}");
        assert!(reply.contains("OK"));
    }

    /// 能力探测必须回显请求的 `i=`，否则客户端认不出这是对它的应答。
    ///
    /// 这条序列是 terminal-browser 实际发的原样拷贝：它只认包含 `Gi=4207;`
    /// 的回复，回 `i=0` 会被判成"此终端不支持图像"。ghostty 的
    /// `graphics_command.zig` 里 `response: encode with only image id` 也是同样
    /// 语义（回显请求 id，`I=` 才是图像序号）。
    #[test]
    fn query_reply_echoes_request_id() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "i=4207,a=q,t=d,f=24,s=1,v=1;AAAA", (0, 0)).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r.reply),
            "\x1b_Gi=4207;OK\x1b\\",
            "探测应答必须带上请求的 id"
        );
    }

    /// 请求没带 id 时仍然回 `i=0`（对齐 ghostty 的默认）。
    #[test]
    fn query_reply_without_id_falls_back_to_zero() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "a=q,f=24,s=1,v=1;AAAA", (0, 0)).unwrap();
        assert_eq!(String::from_utf8_lossy(&r.reply), "\x1b_Gi=0;OK\x1b\\");
    }

    /// 传输与显示同样回显请求 id（客户端据此把答卷和请求对上）。
    #[test]
    fn transmit_reply_echoes_request_id() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(
            &mut store,
            &format!("a=t,i=77,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("i=77;"),
            "回显 id 应为 77: {:?}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn oversized_dimensions_rejected() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "a=t,i=1,f=32,s=100000,v=100000;AAAA", (0, 0)).unwrap();
        let reply = String::from_utf8_lossy(&r.reply);
        assert!(reply.contains("EINVAL"), "超限尺寸应回报 EINVAL: {reply}");
        assert!(store.lock_storage().is_empty());
    }

    #[test]
    fn storage_evicts_within_limit() {
        // 上限 8 字节 = 2 张 1×1 RGBA。插入 3 张应变淘汰。
        let mut store = ImageStore::new(8);
        for id in 1..=3 {
            run(
                &mut store,
                &format!("a=t,i={id},f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
                (0, 0),
            )
            .unwrap();
        }
        assert!(store.total_bytes() <= 8);
        assert!(store.lock_storage().len() <= 2);
    }

    #[test]
    fn chunked_png_decodes_when_split_at_non_aligned_boundary() {
        // 回归测试：分片必须累积 **base64 文本** 再解码。
        //
        // kitty 规定客户端先整体编码、再把编码文本按 4 字节切分。这里故意
        // 在非 4 对齐的位置切（100 字符的 base64 切成 49 + 51），逐片解码会
        // 因位对齐错位产出非法 PNG——正是曾经的真实 bug。
        const B64: &str = concat!(
            "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76LAAAAEUlEQVR4nGNg",
            "YPj/Hz8eEQoAQ1d/gZtbdTsAAAAASUVORK5CYII=",
        );
        assert_eq!(B64.len() % 4, 0, "整串应是合法 base64");
        let (head, tail) = B64.split_at(49);
        assert_ne!(head.len() % 4, 0, "切点必须非 4 对齐才能覆盖本 bug");

        let mut store = ImageStore::new(1024 * 1024);
        // 首片 `m=1`：只累积，不产生应答。
        let r = run(&mut store, &format!("i=7,f=100,a=t,m=1;{head}"), (0, 0)).unwrap();
        assert!(r.reply.is_empty(), "中间片不应有应答");
        assert!(store.lock_storage().get(7).is_none(), "中间片不应落盘");

        // 末片 `m=0`：拼接后解码。
        let r = run(&mut store, &format!("i=7,m=0;{tail}"), (0, 0)).unwrap();
        assert!(String::from_utf8_lossy(&r.reply).contains("OK"));

        let guard = store.lock_storage();
        let stored = guard.get(7).expect("分片 PNG 应解码落盘");
        assert_eq!((stored.image.width, stored.image.height), (8, 8));
        assert_eq!(&stored.image.rgba[..4], &[0, 0, 255, 255], "应解出纯蓝像素");
    }

    #[test]
    fn chunked_transmission_with_spec_aligned_split_also_works() {
        // 规范要求非末片 4 对齐；这条路径此前也工作，回归保护。
        let b64 = rgba_pixel(); // "/wAA/w==" → 8 字符
        let (head, tail) = b64.split_at(4);
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("i=3,f=32,s=1,v=1,a=t,m=1;{head}"),
            (0, 0),
        )
        .unwrap();
        run(&mut store, &format!("i=3,m=0;{tail}"), (0, 0)).unwrap();

        let guard = store.lock_storage();
        let stored = guard.get(3).expect("应解码落盘");
        assert_eq!(&stored.image.rgba[..4], &[255, 0, 0, 255]);
    }

    // ---- 动画：a=f 传输帧 ----

    /// 一个 2×1 的红/蓝图，作为动画的底图。
    const TWO_PIXEL_IMAGE: &str = "a=t,i=1,f=32,s=2,v=1;/wAA/wAA//8=";

    /// 单像素 RGBA 的 base64（用于帧数据）。
    fn pixel_b64(r: u8, g: u8, b: u8, a: u8) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = [r, g, b, a];
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut buf = [0u8; 3];
            buf[..chunk.len()].copy_from_slice(chunk);
            let n = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | buf[2] as u32;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(T[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(T[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn animation_frame_is_appended_and_number_reported() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();

        // 传一帧 1×1 的绿色，放在 x=1（覆盖第二个像素）。
        let green = pixel_b64(0, 255, 0, 255);
        let r = run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1,x=1;{green}"),
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("r=2"),
            "新建帧应回报 r=2: {}",
            String::from_utf8_lossy(&r.reply)
        );

        let guard = store.lock_storage();
        let stored = guard.get(1).unwrap();
        let anim = stored.animation.as_ref().expect("应有动画状态");
        assert_eq!(anim.frames.len(), 1, "应追加一个存储帧");
        assert_eq!(anim.frames[0].gap_ms, 40, "未给 z= 应用默认 40ms");
        // 帧是整幅 2×1。未给 `c=` 时底画是背景色（缺省全透明），因此左像素
        // 是透明的，只有右像素被绿色覆盖。
        assert_eq!(&anim.frames[0].data[0..4], &[0, 0, 0, 0], "未覆盖处是背景");
        assert_eq!(&anim.frames[0].data[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn animation_frame_requires_existing_image() {
        let mut store = ImageStore::new(1024 * 1024);
        let yellow = pixel_b64(255, 255, 0, 255);
        // 图像 9 不存在，且 r=5 不是根帧 → 应报 ENOENT。
        let r = run(
            &mut store,
            &format!("a=f,i=9,r=5,f=32,s=1,v=1;{yellow}"),
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("ENOENT"),
            "未知图像应报 ENOENT: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn first_frame_without_image_is_treated_as_transmit() {
        let mut store = ImageStore::new(1024 * 1024);
        // 图像还不存在，r 未指定 → 视作根帧传输，创建图像。
        let r = run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1;{}", rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&r.reply).contains("OK"));
        assert!(store.lock_storage().get(1).is_some(), "应创建图像");
    }

    #[test]
    fn frame_negative_gap_creates_gapless_frame() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1,z=-1;{green}"),
            (0, 0),
        )
        .unwrap();

        let guard = store.lock_storage();
        assert_eq!(
            guard.get(1).unwrap().animation.as_ref().unwrap().frames[0].gap_ms,
            0,
            "负间隔 → 无间隔帧"
        );
    }

    #[test]
    fn frame_gap_is_honoured_when_positive() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1,z=250;{green}"),
            (0, 0),
        )
        .unwrap();
        let guard = store.lock_storage();
        assert_eq!(
            guard.get(1).unwrap().animation.as_ref().unwrap().frames[0].gap_ms,
            250
        );
    }

    #[test]
    fn frame_edit_hits_existing_frame_by_r() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        run(&mut store, &format!("a=f,i=1,f=32,s=1,v=1;{green}"), (0, 0)).unwrap();

        // 用 r=2 编辑刚建的帧：不应新增帧。
        let blue = pixel_b64(0, 0, 255, 255);
        let r = run(
            &mut store,
            &format!("a=f,i=1,r=2,f=32,s=1,v=1;{blue}"),
            (0, 0),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&r.reply).contains("r=2"));

        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        assert_eq!(anim.frames.len(), 1, "编辑不应新增帧");
        assert_eq!(
            &anim.frames[0].data[0..4],
            &[0, 0, 255, 255],
            "应被蓝色覆盖"
        );
    }

    #[test]
    fn frame_larger_than_image_is_rejected() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        // 2×1 的图像，传 1×2 的帧 → 高度超限。
        let data = pixel_b64(1, 1, 1, 255).repeat(2);
        let r = run(&mut store, &format!("a=f,i=1,f=32,s=1,v=2;{data}"), (0, 0)).unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("EINVAL"),
            "超尺寸帧应报 EINVAL: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn create_frame_from_existing_frame_copies_base() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();

        // 先用 c=1（根帧）作底画新建一帧，再在 (0,0) 覆盖成绿色。
        let green = pixel_b64(0, 255, 0, 255);
        run(
            &mut store,
            &format!("a=f,i=1,c=1,f=32,s=1,v=1,x=0;{green}"),
            (0, 0),
        )
        .unwrap();

        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        assert_eq!(
            &anim.frames[0].data[0..4],
            &[0, 255, 0, 255],
            "左像素被覆盖"
        );
        assert_eq!(
            &anim.frames[0].data[4..8],
            &[0, 0, 255, 255],
            "右像素来自底画"
        );
    }

    #[test]
    fn create_frame_from_missing_base_is_rejected() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        let r = run(
            &mut store,
            &format!("a=f,i=1,c=99,f=32,s=1,v=1;{green}"),
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("EINVAL"),
            "不存在的底画帧应报 EINVAL: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn frame_background_color_fills_canvas() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();

        // Y=4278255360（0xff00ff00）→ r=0（最低字节）、g=0xff、b=0、a=0xff。
        let green = pixel_b64(9, 9, 9, 255);
        run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1,Y=4278255360;{green}"),
            (0, 0),
        )
        .unwrap();

        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        // 左像素被覆盖，右像素保留背景色（r=0,g=255,b=0,a=255）。
        assert_eq!(&anim.frames[0].data[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn chunked_animation_frame_assembles() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();

        // 把 1×1 的 base64 切成两片传。
        let full = pixel_b64(0, 255, 0, 255);
        let (head, tail) = full.split_at(4);
        let r = run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1,m=1;{head}"),
            (0, 0),
        )
        .unwrap();
        assert!(r.reply.is_empty(), "中间片不应答");

        let r = run(&mut store, &format!("a=f,i=1,m=0;{tail}"), (0, 0)).unwrap();
        assert!(String::from_utf8_lossy(&r.reply).contains("OK"));

        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        assert_eq!(
            &anim.frames[0].data[0..4],
            &[0, 255, 0, 255],
            "分片应正确拼接"
        );
    }

    #[test]
    fn frames_are_evicted_when_storage_is_full() {
        // 上限 11 字节：底图 1×1（4）+ 占位图 1×1（4）= 8，再预留 4 字节
        // （帧与底图同尺寸）就会超限，因而必须淘汰占位图。
        let mut store = ImageStore::new(11);
        run(
            &mut store,
            &format!("a=t,i=1,f=32,s=1,v=1;{}", rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        // 未放置的临时图像 → 淘汰优先级最低。
        run(
            &mut store,
            &format!("a=t,i=2,f=32,s=1,v=1;{}", rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert_eq!(store.total_bytes(), 8);

        let r = run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1;{}", pixel_b64(0, 255, 0, 255)),
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("OK"),
            "应为帧腾出空间: {}",
            String::from_utf8_lossy(&r.reply)
        );

        let guard = store.lock_storage();
        assert!(guard.get(1).is_some(), "受益图像必须保留");
        assert!(guard.get(2).is_none(), "未放置的临时图像应被淘汰");
        assert_eq!(
            guard
                .get(1)
                .unwrap()
                .animation
                .as_ref()
                .unwrap()
                .frames
                .len(),
            1
        );
    }

    #[test]
    fn failed_frame_reservation_leaves_storage_intact() {
        // 底图 2×1（8 字节）+ 占位图 1×1（4 字节），上限 13（刚好放得下）。
        let mut store = ImageStore::new(13);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(
            &mut store,
            &format!("a=t,i=2,f=32,s=1,v=1;{}", rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        assert_eq!(store.total_bytes(), 12);

        // 底图是 2×1，帧也必须是整幅 8 字节。淘汰占位图（4 字节）后是
        // 8 + 8 = 16 > 13，仍然不够 → 应失败。
        let r = run(
            &mut store,
            &format!("a=f,i=1,f=32,s=1,v=1;{}", pixel_b64(0, 255, 0, 255)),
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("ENOSPC"),
            "腾不出空间应报 ENOSPC: {}",
            String::from_utf8_lossy(&r.reply)
        );

        // 关键性质：失败不应把其他图像当成牺牲品淘汰掉。
        let guard = store.lock_storage();
        assert!(guard.get(1).is_some(), "底图应保留");
        assert!(guard.get(2).is_some(), "失败时不应牺牲其他图像");
        assert_eq!(guard.total_bytes(), 12, "失败时字节计数不应变化");
    }

    #[test]
    fn frame_storage_full_reports_enospc() {
        // 上限刚好只放得下底图，没有任何可淘汰的图像。
        let mut store = ImageStore::new(8);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        let r = run(&mut store, &format!("a=f,i=1,f=32,s=1,v=1;{green}"), (0, 0)).unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("ENOSPC"),
            "腾不出空间应报 ENOSPC: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    // ---- 动画：a=a 播放控制 ----

    fn anim_state(store: &ImageStore) -> (u32, State, u32, u32) {
        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        (
            anim.current_index,
            anim.state,
            anim.max_loops,
            anim.current_loop,
        )
    }

    #[test]
    fn control_sets_running_state() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(&mut store, "a=a,i=1,s=3", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).1, State::Running);
    }

    #[test]
    fn control_run_wait_maps_to_loading() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(&mut store, "a=a,i=1,s=2", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).1, State::Loading);
    }

    #[test]
    fn control_unknown_state_is_ignored() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(&mut store, "a=a,i=1,s=3", (0, 0)).unwrap();
        // s=9 未知 → 保持原状态。
        run(&mut store, "a=a,i=1,s=9", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).1, State::Running);
    }

    #[test]
    fn control_sets_current_frame() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        run(&mut store, &format!("a=f,i=1,f=32,s=1,v=1;{green}"), (0, 0)).unwrap();

        run(&mut store, "a=a,i=1,c=2", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).0, 1, "c=2 应选中零基下标 1");
    }

    #[test]
    fn control_out_of_range_frame_is_ignored() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(&mut store, "a=a,i=1,c=99", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).0, 0, "越界帧号应被忽略");
    }

    #[test]
    fn control_sets_root_gap() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(&mut store, "a=a,i=1,r=1,z=100", (0, 0)).unwrap();

        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        assert_eq!(anim.root_gap_ms, 100, "r=1,z= 应给根帧设间隔");
    }

    #[test]
    fn control_loops_are_stored_off_by_one() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        run(&mut store, "a=a,i=1,v=1", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).2, 0, "v=1 表示无限循环 → max_loops=0");

        run(&mut store, "a=a,i=1,v=3", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).2, 2, "v=3 → max_loops=2");
    }

    #[test]
    fn control_success_is_silent() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let r = run(&mut store, "a=a,i=1,s=3", (0, 0)).unwrap();
        assert!(r.reply.is_empty(), "成功的动画控制不应答");
    }

    #[test]
    fn control_on_missing_image_reports_enoent() {
        let mut store = ImageStore::new(1024 * 1024);
        let r = run(&mut store, "a=a,i=77,s=3", (0, 0)).unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("ENOENT"),
            "未知图像应报 ENOENT: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn control_state_change_resets_loop_counter() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        {
            let mut guard = store.lock_storage();
            let anim = guard
                .get_mut(1)
                .unwrap()
                .animation
                .get_or_insert_with(Animation::default);
            anim.current_loop = 5;
        }
        run(&mut store, "a=a,i=1,s=3", (0, 0)).unwrap();
        assert_eq!(anim_state(&store).3, 0, "状态变化应重置轮数");
    }

    // ---- 动画：a=c 帧间合成 ----

    #[test]
    fn compose_copies_rect_between_frames() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();

        // 建一个全透明帧（Y 缺省 → 全 0 背景）。
        let green = pixel_b64(0, 255, 0, 255);
        run(&mut store, &format!("a=f,i=1,f=32,s=1,v=1;{green}"), (0, 0)).unwrap();

        // 把根帧（c=1 源）左侧 1 像素拷到帧 2（c=2 目标）的右侧。
        // 注意 r 是源、c 是目标——规范表格写反了。
        let r = run(
            &mut store,
            "a=c,i=1,r=1,c=2,x=1,y=0,w=1,h=1,X=0,Y=0,C=1",
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).is_empty(),
            "成功的 a=c 不应答"
        );

        let guard = store.lock_storage();
        let anim = guard.get(1).unwrap().animation.as_ref().unwrap();
        assert_eq!(
            &anim.frames[0].data[4..8],
            &[255, 0, 0, 255],
            "右像素应被根帧左像素覆盖"
        );
    }

    #[test]
    fn compose_out_of_bounds_rectangle_is_rejected() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        // 目标矩形 x=1,w=2 超出 2 像素宽的图像。
        let r = run(&mut store, "a=c,i=1,r=1,c=1,x=1,w=2,h=1,X=0,Y=0", (0, 0)).unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("EINVAL"),
            "越界矩形应报 EINVAL: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn compose_self_overlap_is_rejected() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        // 源与目标都是帧 1，矩形完全重叠。
        let r = run(
            &mut store,
            "a=c,i=1,r=1,c=1,x=0,y=0,w=1,h=1,X=0,Y=0",
            (0, 0),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("EINVAL"),
            "自合成且矩形重叠应报 EINVAL: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    #[test]
    fn compose_self_non_overlapping_is_allowed() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        // 把 (0,0) 拷到 (1,0)：同帧但不重叠。
        let r = run(
            &mut store,
            "a=c,i=1,r=1,c=1,x=1,y=0,w=1,h=1,X=0,Y=0,C=1",
            (0, 0),
        )
        .unwrap();
        assert!(r.reply.is_empty());

        let guard = store.lock_storage();
        let stored = guard.get(1).unwrap();
        assert_eq!(
            &stored.image.rgba[4..8],
            &[255, 0, 0, 255],
            "右像素应变成红色"
        );
    }

    #[test]
    fn compose_missing_frame_reports_enoent() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let r = run(&mut store, "a=c,i=1,r=5,c=1,w=1,h=1", (0, 0)).unwrap();
        assert!(
            String::from_utf8_lossy(&r.reply).contains("ENOENT"),
            "不存在的帧应报 ENOENT: {}",
            String::from_utf8_lossy(&r.reply)
        );
    }

    // ---- 动画：d=f 删除帧 ----

    #[test]
    fn delete_frames_target_removes_one_frame() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let green = pixel_b64(0, 255, 0, 255);
        run(&mut store, &format!("a=f,i=1,f=32,s=1,v=1;{green}"), (0, 0)).unwrap();
        run(&mut store, &format!("a=f,i=1,f=32,s=1,v=1;{green}"), (0, 0)).unwrap();

        let r = run(&mut store, "a=d,d=f,i=1,r=3", (0, 0)).unwrap();
        assert!(r.images_changed);

        let guard = store.lock_storage();
        assert_eq!(
            guard
                .get(1)
                .unwrap()
                .animation
                .as_ref()
                .unwrap()
                .frames
                .len(),
            1,
            "应删掉一个帧"
        );
    }

    #[test]
    fn delete_frames_uppercase_removes_non_animated_image() {
        let mut store = ImageStore::new(1024 * 1024);
        run(&mut store, TWO_PIXEL_IMAGE, (0, 0)).unwrap();
        let r = run(&mut store, "a=d,d=F,i=1", (0, 0)).unwrap();
        assert!(r.images_changed);
        assert!(
            store.lock_storage().get(1).is_none(),
            "d=F 应删除非动画图像"
        );
    }

    // ---- 虚拟放置（U=1）----

    #[test]
    fn display_with_uppercase_u_creates_virtual_placement() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=t,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();

        // U=1 建虚拟放置；c=/r= 声明铺进 4x2 格。
        run(&mut store, "a=p,i=1,U=1,c=4,r=2,q=2", (0, 0)).unwrap();

        let guard = store.lock_storage();
        let stored = guard.get(1).unwrap();
        let placement = stored.placements.get(&0).expect("放置应存在");
        assert!(placement.is_virtual(), "U=1 应建虚拟放置");
        assert_eq!(placement.columns, Some(4));
        assert_eq!(placement.rows, Some(2));
    }

    #[test]
    fn display_without_u_creates_anchor_placement() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=t,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        run(&mut store, "a=p,i=1,c=4,r=2", (2, 3)).unwrap();

        let guard = store.lock_storage();
        let placement = guard.get(1).unwrap().placements.get(&0).unwrap();
        assert!(!placement.is_virtual(), "默认应是锚定放置");
        assert_eq!((placement.row, placement.col), (2, 3));
    }

    #[test]
    fn display_with_u_zero_creates_anchor_placement() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=t,i=1,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        // U=0 是显式的"非虚拟"，不能因为键存在就当虚拟处理。
        run(&mut store, "a=p,i=1,U=0", (0, 0)).unwrap();

        let guard = store.lock_storage();
        assert!(
            !guard
                .get(1)
                .unwrap()
                .placements
                .get(&0)
                .unwrap()
                .is_virtual()
        );
    }

    #[test]
    fn virtual_placement_is_resolvable_as_placeholder_target() {
        let mut store = ImageStore::new(1024 * 1024);
        run(
            &mut store,
            &format!("a=t,i=7,f=32,s=1,v=1;{pixel}", pixel = rgba_pixel()),
            (0, 0),
        )
        .unwrap();
        run(&mut store, "a=p,i=7,U=1,c=3,r=3", (0, 0)).unwrap();

        let guard = store.lock_storage();
        // 未指定放置 id → 取第一个虚拟放置。
        let (pid, placement) = guard.placeholder_target(7, 0).expect("应能解析占位符目标");
        assert_eq!(pid, 0);
        assert_eq!(placement.columns, Some(3));
    }
}
