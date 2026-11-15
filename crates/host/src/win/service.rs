//! 系统服务模式。
//!
//! 服务本身运行在 session 0，看不到任何人的屏幕。它负责在当前控制台会话（有显示器的那个
//! 登录会话）里用 SYSTEM 身份启动“工作进程”——也就是普通的被控端主程序。SYSTEM 身份让工作进程
//! 可以进入锁屏 / 登录界面 / UAC 所在的安全桌面，采集并注入输入（见 `desktop.rs`）。
//!
//! 服务会守护工作进程：进程退出就重启；用户注销、切换用户导致控制台会话变化时，
//! 结束旧进程并在新会话里重新启动。
//!
//! 目录：
//! - 程序：`%ProgramFiles%\RemoteControl\rc-host.exe`
//! - 数据：`%ProgramData%\RemoteControl\`（密码与日志；仅 SYSTEM 和管理员可读）

use crate::Args;
use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, SECURITY_ATTRIBUTES,
    TOKEN_ALL_ACCESS,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
    ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "RemoteControlHost";
const DISPLAY_NAME: &str = "RemoteControl 被控端";
const DESCRIPTION: &str = "局域网远程控制被控端：采集屏幕并接受远程键鼠操作，可控制锁屏与 UAC 界面。";
const FIREWALL_RULE: &str = "RemoteControlHost";
const NO_SESSION: u32 = u32::MAX;

fn program_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into())).join("RemoteControl")
}

pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("RemoteControl")
}

pub fn log_file() -> PathBuf {
    data_dir().join("host.log")
}

/// 传给工作进程的参数。
fn worker_args(args: &Args) -> Vec<String> {
    let mut v = vec!["--port".into(), args.port.to_string(), "--bitrate".into(), args.bitrate.to_string(), "--fec".into(), args.fec.to_string()];
    if let Some(name) = &args.name {
        v.extend(["--name".into(), name.clone()]);
    }
    if let Some(fps) = args.fps {
        v.extend(["--fps".into(), fps.to_string()]);
    }
    if args.allow_sleep {
        v.push("--allow-sleep".into());
    }
    v
}

/// 服务自身的日志（工作进程的输出也重定向到同一个文件）。
fn log(msg: &str) {
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}] [服务] {msg}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_file()) {
        let _ = f.write_all(line.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// 服务运行
// ---------------------------------------------------------------------------

static SERVICE_ARGS: OnceLock<Args> = OnceLock::new();

/// `rc-host service`：由服务管理器启动时的入口。
pub fn run(args: Args) -> Result<()> {
    let _ = SERVICE_ARGS.set(args);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("该命令只能由 Windows 服务管理器启动；安装服务请使用 `rc-host install`")?;
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = service_body() {
        log(&format!("服务异常退出: {e:#}"));
    }
}

fn service_body() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state: ServiceState, accept: ServiceControlAccept| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN)?;

    let result = (|| -> Result<()> {
        let args = SERVICE_ARGS.get().context("缺少服务参数")?;
        log(&format!("服务已启动，端口 {}", args.port));
        supervise(args, &stop_rx);
        Ok(())
    })();
    if let Err(e) = &result {
        log(&format!("{e:#}"));
    }

    log("服务已停止");
    set(ServiceState::Stopped, ServiceControlAccept::empty())?;
    result
}

struct Worker {
    process: HANDLE,
    session: u32,
    started: Instant,
}

impl Worker {
    fn exit_code(&self) -> Option<u32> {
        unsafe {
            if WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            let _ = GetExitCodeProcess(self.process, &mut code);
            Some(code)
        }
    }

    fn kill(self) {
        unsafe {
            if self.exit_code().is_none() {
                let _ = TerminateProcess(self.process, 1);
                WaitForSingleObject(self.process, 5000);
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.process);
        }
    }
}

fn supervise(args: &Args, stop: &mpsc::Receiver<()>) {
    let mut worker: Option<Worker> = None;
    let mut failures = 0u32;
    let mut next_start = Instant::now();

    loop {
        match stop.recv_timeout(Duration::from_millis(500)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let session = unsafe { WTSGetActiveConsoleSessionId() };

        if let Some(w) = &worker {
            if let Some(code) = w.exit_code() {
                // 很快就退出（例如端口被占用、显卡初始化失败）时逐步拉长重试间隔，避免刷屏。
                failures = if w.started.elapsed() < Duration::from_secs(10) { failures + 1 } else { 0 };
                let delay = Duration::from_secs(1 << failures.min(4));
                log(&format!("工作进程已退出（代码 {code}），{} 秒后重启", delay.as_secs()));
                next_start = Instant::now() + delay;
                worker = None;
            } else if w.session != session {
                log(&format!("控制台会话已变化（{} → {}），重启工作进程", w.session, session));
                worker.take().unwrap().kill();
                failures = 0;
                next_start = Instant::now();
            }
        }

        if worker.is_none() && session != NO_SESSION && Instant::now() >= next_start {
            match spawn_worker(args, session) {
                Ok(w) => {
                    log(&format!("已在会话 {session} 中启动工作进程"));
                    worker = Some(w);
                }
                Err(e) => {
                    failures += 1;
                    let delay = Duration::from_secs(1 << failures.min(4));
                    log(&format!("启动工作进程失败: {e:#}，{} 秒后重试", delay.as_secs()));
                    next_start = Instant::now() + delay;
                }
            }
        }
    }

    if let Some(w) = worker {
        log("正在结束工作进程");
        w.kill();
    }
}

/// 日志超过 5MB 时轮换成 host.old.log。
fn rotate_log() {
    let path = log_file();
    if std::fs::metadata(&path).map(|m| m.len() > 5 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&path, data_dir().join("host.old.log"));
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain([0]).collect()
}

/// 以 SYSTEM 身份、在指定会话中启动工作进程，标准输出/错误重定向到日志文件。
fn spawn_worker(args: &Args, session: u32) -> Result<Worker> {
    rotate_log();
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).context("OpenProcessToken")?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        dup.context("DuplicateTokenEx")?;

        let result = (|| -> Result<Worker> {
            // 令牌仍是 SYSTEM，只是把它“搬”到用户的会话里。
            SetTokenInformation(token, TokenSessionId, (&session as *const u32).cast(), 4).context("SetTokenInformation")?;

            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: true.into(),
            };
            let log_path = wide(log_file().as_os_str());
            let log_handle = CreateFileW(
                PCWSTR(log_path.as_ptr()),
                FILE_APPEND_DATA.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                Some(&sa),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .context("打开日志文件")?;

            let mut desktop = wide(OsStr::new(r"winsta0\default"));
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: HANDLE::default(),
                hStdOutput: log_handle,
                hStdError: log_handle,
                ..Default::default()
            };
            let exe = std::env::current_exe()?;
            let quoted: Vec<String> = worker_args(args)
                .into_iter()
                .map(|a| if a.contains(' ') || a.is_empty() { format!("\"{}\"", a.replace('"', "\\\"")) } else { a })
                .collect();
            let cmdline = format!("\"{}\" {}", exe.display(), quoted.join(" "));
            let mut cmdline = wide(OsStr::new(&cmdline));
            let mut info = PROCESS_INFORMATION::default();
            let created = CreateProcessAsUserW(
                Some(token),
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                true, // 继承日志文件句柄
                CREATE_NO_WINDOW,
                None, // 继承服务的环境变量（含 RC_PASSWORD）
                PCWSTR::null(),
                &startup,
                &mut info,
            );
            let _ = CloseHandle(log_handle);
            created.context("CreateProcessAsUserW")?;
            let _ = CloseHandle(info.hThread);
            Ok(Worker { process: info.hProcess, session, started: Instant::now() })
        })();
        let _ = CloseHandle(token);
        result
    }
}

// ---------------------------------------------------------------------------
// 安装 / 卸载
// ---------------------------------------------------------------------------

fn admin_hint(e: windows_service::Error) -> anyhow::Error {
    if let windows_service::Error::Winapi(io) = &e {
        if io.raw_os_error() == Some(5) {
            return anyhow::anyhow!("需要管理员权限：请在开始菜单搜索“终端”，右键选择“以管理员身份运行”，再执行本命令");
        }
    }
    anyhow::Error::new(e)
}

/// 停止服务并等待其完全退出（确保程序文件不再被占用）。
fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
    let state = service.query_status()?.current_state;
    if state == ServiceState::Stopped {
        return Ok(());
    }
    println!("正在停止已运行的服务…");
    let _ = service.stop();
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.query_status()?.current_state != ServiceState::Stopped {
        if Instant::now() > deadline {
            bail!("等待服务停止超时，请稍后重试");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

/// 运行一段 PowerShell，返回其标准输出（强制 UTF-8，避免中文系统下的 GBK 乱码）。
fn powershell(script: &str) -> String {
    let script = format!("[Console]::OutputEncoding = [Text.Encoding]::UTF8; $ErrorActionPreference = 'SilentlyContinue'; {script}");
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// 各网卡的网络类型：(网卡名, Public/Private/DomainAuthenticated)。
fn network_profiles() -> Vec<(String, String)> {
    powershell("Get-NetConnectionProfile | ForEach-Object { \"$($_.InterfaceAlias)`t$($_.NetworkCategory)\" }")
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(a, b)| (a.trim().to_string(), b.trim().to_string())))
        .collect()
}

/// 带有局域网地址（10/8、172.16/12、192.168/16）的网卡。代理软件的 TUN 网卡（如 198.18.x）、
/// Tailscale（100.x）等不在其中——它们是不是“公用网络”与局域网连接无关。
fn lan_interfaces() -> std::collections::HashSet<String> {
    powershell("Get-NetIPAddress -AddressFamily IPv4 | ForEach-Object { \"$($_.InterfaceAlias)`t$($_.IPAddress)\" }")
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, ip)| ip.trim().parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_private()))
        .map(|(nic, _)| nic.trim().to_string())
        .collect()
}

/// 被识别为公用网络的局域网网卡。
fn public_lan_interfaces() -> Vec<String> {
    let lan = lan_interfaces();
    network_profiles().into_iter().filter(|(nic, cat)| cat == "Public" && lan.contains(nic)).map(|(nic, _)| nic).collect()
}

const PUBLIC_NETWORK_HINT: &str = "防火墙规则只对“专用网络”生效，公用网络下控制端连不上。\n\
     修改方法：设置 → 网络和 Internet → 以太网 / WLAN → 当前网络的“属性” → 网络配置文件类型选“专用网络”。";

/// 静默运行一个系统命令，返回是否成功。
fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

/// netsh 要求 `program="C:\Program Files\..."` 这种写法，参数原样传入，不做额外转义。
fn netsh(args: &[&str]) -> bool {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("netsh");
    for a in args {
        cmd.raw_arg(a);
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 复制程序文件；刚停止的服务可能还短暂占用文件，所以重试几次。
fn copy_exe(from: &Path, to: &Path) -> Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match std::fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(last.unwrap()).with_context(|| format!("复制程序到 {} 失败", to.display()))
}

pub fn install(args: &Args) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(admin_hint)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG;
    let existing = manager.open_service(SERVICE_NAME, access).ok();
    if let Some(s) = &existing {
        stop_and_wait(s)?;
    }

    // 1. 程序复制到 Program Files，之后删掉下载的文件也不影响服务。
    let dir = program_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("创建 {}", dir.display()))?;
    let exe = dir.join("rc-host.exe");
    let me = std::env::current_exe()?;
    if !same_path(&me, &exe) {
        copy_exe(&me, &exe)?;
    }

    // 2. 数据目录只允许 SYSTEM 与管理员访问（用 SID，避免本地化组名的问题）。
    let data = data_dir();
    std::fs::create_dir_all(&data)?;
    let data_str = data.to_string_lossy();
    if !quiet("icacls", &[&data_str, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"]) {
        eprintln!("警告：设置 {} 的访问权限失败，配对信息可能对其他用户可读", data.display());
    }
    // 旧版本的密码文件已不再使用
    let _ = std::fs::remove_file(data.join("password.txt"));

    // 3. 防火墙：按程序放行入站（专用 / 域网络）。服务没有界面，不会弹出防火墙询问框。
    let exe_str = exe.to_string_lossy();
    netsh(&["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]);
    if !netsh(&[
        "advfirewall",
        "firewall",
        "add",
        "rule",
        &format!("name={FIREWALL_RULE}"),
        "dir=in",
        "action=allow",
        &format!("program=\"{exe_str}\""),
        "enable=yes",
        "profile=private,domain",
    ]) {
        eprintln!("警告：添加防火墙规则失败，可能需要手动放行 TCP {} 入站", args.port);
    }
    let public = public_lan_interfaces();

    // 4. 创建（或更新）服务：开机自动启动，以 LocalSystem 运行，崩溃后自动重启。
    let mut launch_arguments: Vec<OsString> = vec!["service".into()];
    launch_arguments.extend(worker_args(args).into_iter().map(OsString::from));
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments,
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service = match existing {
        Some(s) => {
            s.change_config(&info)?;
            s
        }
        None => manager.create_service(&info, access)?,
    };
    service.set_description(DESCRIPTION)?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some((0..3).map(|_| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) }).collect()),
    })?;
    service.start(&[] as &[&OsStr]).context("启动服务失败")?;

    println!("已安装并启动系统服务「{DISPLAY_NAME}」，开机自动运行。");
    println!();
    println!("  本机名称：{}", args.name.clone().unwrap_or_else(crate::trust::computer_name));
    println!("  端口：    {}", args.port);
    println!("  程序位置：{}", exe.display());
    println!("  运行日志：{}", log_file().display());
    println!();
    println!("控制端打开后会自动发现这台电脑。新设备第一次连接时，这里会弹窗请你确认配对码；");
    println!("确认后该设备以后可以直接连接。锁屏、登录界面和 UAC 弹窗也能看到并操作。");
    println!("提示：如果之前手动运行的 rc-host 还开着，请先关掉它，否则端口会冲突。");
    println!("修改参数：重新执行 install 并带上新参数；查看/删除已配对设备：rc-host devices / rc-host forget <名称>；卸载：rc-host uninstall");
    println!("连不上时运行 rc-host status（或双击 diagnose.cmd）检查问题。");
    if !public.is_empty() {
        println!();
        println!("⚠️  注意：网络「{}」当前是“公用网络”。", public.join("、"));
        println!("{PUBLIC_NETWORK_HINT}");
    }
    Ok(())
}

/// `rc-host status`：检查服务、端口、网络类型与防火墙，并显示最近的日志。
pub fn status() -> Result<()> {
    let mut problems: Vec<String> = Vec::new();

    // 1. 服务
    println!("== 系统服务 ==");
    let mut port = rc_protocol::DEFAULT_PORT;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(admin_hint)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG) {
        Ok(service) => {
            let st = service.query_status()?;
            let state = match st.current_state {
                ServiceState::Running => "运行中",
                ServiceState::Stopped => "已停止",
                ServiceState::StartPending => "正在启动",
                ServiceState::StopPending => "正在停止",
                _ => "其它",
            };
            println!("状态：{state}");
            if let Ok(cfg) = service.query_config() {
                let cmdline = cfg.executable_path.to_string_lossy().to_string();
                println!("命令：{cmdline}");
                let mut it = cmdline.split_whitespace();
                while let Some(tok) = it.next() {
                    if tok == "--port" {
                        if let Some(p) = it.next().and_then(|p| p.parse().ok()) {
                            port = p;
                        }
                    }
                }
            }
            if st.current_state != ServiceState::Running {
                problems.push("服务没有运行：重新双击 install-service.cmd，或在“服务”管理器里启动「RemoteControl 被控端」".into());
            }
        }
        Err(_) => {
            println!("状态：未安装");
            problems.push("系统服务未安装：双击 install-service.cmd 安装".into());
        }
    }

    // 2. 端口是否在监听（能连上本机端口，说明被控端进程正常在等连接）
    println!();
    println!("== 端口 {port} ==");
    let listening = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)).is_ok();
    println!("{}", if listening { "正在监听" } else { "没有监听" });
    if !listening {
        problems.push(format!("端口 {port} 没有在监听：被控端进程没能正常运行，请看下方日志中的错误"));
    }
    let procs = powershell("Get-Process rc-host | ForEach-Object { \"PID $($_.Id)  会话 $($_.SessionId)  $($_.Path)\" }");
    if !procs.is_empty() {
        println!("rc-host 进程：");
        for l in procs.lines() {
            println!("  {l}");
        }
    }

    // 3. 网络与防火墙
    println!();
    println!("== 网络 ==");
    let ips = powershell(
        "Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' } | ForEach-Object { \"$($_.InterfaceAlias)`t$($_.IPAddress)\" }",
    );
    for l in ips.lines() {
        if let Some((nic, ip)) = l.split_once('\t') {
            println!("本机地址：{ip}（{nic}）");
        }
    }
    let lan = lan_interfaces();
    for (nic, cat) in network_profiles() {
        let is_lan = lan.contains(&nic);
        let name = match cat.as_str() {
            "Public" if is_lan => "公用网络 ⚠️",
            "Public" => "公用网络（不是局域网网卡，不影响）",
            "Private" => "专用网络",
            "DomainAuthenticated" => "域网络",
            other => other,
        };
        println!("网络类型：{nic} → {name}");
        if cat == "Public" && is_lan {
            problems.push(format!("网络「{nic}」是公用网络。{PUBLIC_NETWORK_HINT}"));
        }
    }
    let rule = powershell(&format!(
        "Get-NetFirewallRule -DisplayName '{FIREWALL_RULE}' | ForEach-Object {{ \"启用 $($_.Enabled)  动作 $($_.Action)  网络 $($_.Profile)\" }}"
    ));
    if rule.is_empty() {
        println!("防火墙规则：不存在");
        problems.push("防火墙规则不存在：重新运行 install".into());
    } else {
        println!("防火墙规则：{rule}");
    }

    // 4. 日志
    println!();
    println!("== 已配对的控制端 ==");
    if let Err(e) = crate::list_devices() {
        println!("（读取失败：{e:#}，需要管理员权限）");
    }

    println!();
    println!("== 最近日志（{}）==", log_file().display());
    match std::fs::read(log_file()) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            for l in &lines[lines.len().saturating_sub(30)..] {
                println!("{l}");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!("（需要管理员权限才能查看，请双击 diagnose.cmd）");
        }
        Err(_) => println!("（暂无日志）"),
    }

    println!();
    if problems.is_empty() {
        println!("✅ 被控端这边一切正常。如果 Mac 仍连不上，请确认两台机器在同一网段、地址填的是上面的“本机地址”，");
        println!("   并在 Mac 上执行  nc -vz <本机地址> {port}  看端口是否可达。");
    } else {
        println!("发现以下问题：");
        for (i, p) in problems.iter().enumerate() {
            println!("{}. {p}", i + 1);
        }
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(admin_hint)?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE) {
        Ok(s) => {
            stop_and_wait(&s)?;
            s.delete().map_err(admin_hint)?;
            println!("已删除系统服务「{DISPLAY_NAME}」");
        }
        Err(e) => {
            let e = admin_hint(e);
            if e.to_string().contains("管理员") {
                return Err(e);
            }
            println!("系统服务未安装，继续清理其余文件");
        }
    }

    netsh(&["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_RULE}")]);
    let _ = std::fs::remove_dir_all(data_dir());

    let dir = program_dir();
    let me = std::env::current_exe()?;
    let running_from_install_dir = me.parent().is_some_and(|p| same_path(p, &dir));
    if dir.exists() && (running_from_install_dir || std::fs::remove_dir_all(&dir).is_err()) {
        println!("程序目录 {} 正在使用，请稍后手动删除", dir.display());
    }
    println!("卸载完成");
    Ok(())
}
