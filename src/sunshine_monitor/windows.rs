use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use std::mem;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE, MAX_PATH};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    EnumProcessModulesEx, GetModuleFileNameExW, LIST_MODULES_ALL,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};

use super::SunshineSettingsEvent;

// ── Constants ──────────────────────────────────────────────────────────────────

const SUNSHINE_EXE_NAME: &str = "sunshine.exe";

/// Template for downloading debug info from GitHub releases.
/// `{VERSION}` is replaced with the detected Sunshine version string.
const DEBUGINFO_URL_TEMPLATE: &str =
    "https://github.com/LizardByte/Sunshine/releases/download/v{VERSION}/Sunshine-Windows-AMD64-debuginfo.7z";

/// Default Sunshine version (used when version detection fails).
const DEFAULT_VERSION: &str = "2025.924.154138";

/// How often (ms) to poll the running Sunshine process for `display_cursor`.
const POLL_INTERVAL_MS: u64 = 2000;

/// How long to wait (seconds) before retrying when Sunshine is not found.
const RETRY_INTERVAL_SECS: u64 = 5;

// ── RAII handle wrapper ────────────────────────────────────────────────────────

struct SafeHandle(HANDLE);
impl Drop for SafeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// ── Process discovery ──────────────────────────────────────────────────────────

/// Information about the running Sunshine process.
struct SunshineProcess {
    pid: u32,
    exe_path: PathBuf,
    module_base: usize,
}

/// Find the running Sunshine process, returning its PID, path, and module base.
fn find_sunshine_process() -> Result<Option<SunshineProcess>> {
    unsafe {
        // Snapshot all processes
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .context("CreateToolhelp32Snapshot failed")?;
        let _snap_guard = SafeHandle(snapshot);

        let mut entry = PROCESSENTRY32W {
            dwSize: mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snapshot, &mut entry).is_err() {
            return Ok(None);
        }

        loop {
            let name_end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);

            if name.eq_ignore_ascii_case(SUNSHINE_EXE_NAME) {
                let pid = entry.th32ProcessID;

                // Open with read access
                let handle = OpenProcess(
                    PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                    false,
                    pid,
                )?;
                let _h_guard = SafeHandle(handle);

                // Enumerate modules to get base address and full path
                let mut modules =
                    [windows::Win32::Foundation::HMODULE::default(); 1];
                let mut needed = 0u32;
                if EnumProcessModulesEx(
                    handle,
                    modules.as_mut_ptr(),
                    mem::size_of_val(&modules) as u32,
                    &mut needed,
                    LIST_MODULES_ALL,
                )
                .is_ok()
                {
                    let base_addr = modules[0].0 as usize;

                    // Get full path of the main module
                    let mut name_buf = [0u16; MAX_PATH as usize];
                    let len = GetModuleFileNameExW(handle, modules[0], &mut name_buf);
                    let exe_path = if len > 0 {
                        PathBuf::from(String::from_utf16_lossy(&name_buf[..len as usize]))
                    } else {
                        PathBuf::new()
                    };

                    return Ok(Some(SunshineProcess {
                        pid,
                        exe_path,
                        module_base: base_addr,
                    }));
                }

                return Ok(Some(SunshineProcess {
                    pid,
                    exe_path: PathBuf::new(),
                    module_base: 0,
                }));
            }

            if Process32NextW(snapshot, &mut entry).is_err() {
                break;
            }
        }

        Ok(None)
    }
}

// ── Version detection ──────────────────────────────────────────────────────────

/// Try to determine the Sunshine version from the installed executable.
///
/// Reads the `ProductVersion` resource string from the PE version info block.
/// Falls back to `DEFAULT_VERSION` on any failure.
fn detect_sunshine_version(exe_path: &Path) -> String {
    if !exe_path.exists() {
        return DEFAULT_VERSION.to_string();
    }

    match read_product_version(exe_path) {
        Ok(ver) => ver,
        Err(e) => {
            warn!("Cannot read Sunshine version from {:?}: {}", exe_path, e);
            DEFAULT_VERSION.to_string()
        }
    }
}

/// Read the `ProductVersion` string from a PE file's version-info resource.
fn read_product_version(exe_path: &Path) -> Result<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    use windows::core::PCWSTR;

    let path_wide: Vec<u16> = exe_path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut _handle = 0u32;
        let size = GetFileVersionInfoSizeW(PCWSTR(path_wide.as_ptr()), Some(&mut _handle));
        if size == 0 {
            return Err(anyhow!("GetFileVersionInfoSizeW returned 0"));
        }

        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(
            PCWSTR(path_wide.as_ptr()),
            _handle,
            size,
            buf.as_mut_ptr() as *mut c_void,
        )
        .context("GetFileVersionInfoW")?;

        // Query the root VS_FIXEDFILEINFO to get numeric version
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let sub_block: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();

        let vq_result = VerQueryValueW(
            buf.as_ptr() as *const c_void,
            PCWSTR(sub_block.as_ptr()),
            &mut ptr,
            &mut len,
        );
        if !vq_result.as_bool() {
            return Err(anyhow!("VerQueryValueW failed"));
        }

        if ptr.is_null() || len == 0 {
            return Err(anyhow!("VS_FIXEDFILEINFO not found"));
        }

        // VS_FIXEDFILEINFO layout (we only need the first 4 DWORDs after signature)
        #[repr(C)]
        #[derive(Debug)]
        struct VsFixedFileInfo {
            dw_signature: u32,
            dw_struc_version: u32,
            dw_file_version_ms: u32,
            dw_file_version_ls: u32,
        }

        let info = &*(ptr as *const VsFixedFileInfo);
        if info.dw_signature != 0xFEEF04BD {
            return Err(anyhow!("Invalid VS_FIXEDFILEINFO signature"));
        }

        // Sunshine uses format "YYYY.DDD.HHMMSS" which maps to:
        //   major = YYYY, minor = DDD, build = 0, private_build = HHMMSS
        // But the numeric version is stored traditionally as:
        //   MS = (major << 16) | minor
        //   LS = (build << 16) | private_build
        let major = (info.dw_file_version_ms >> 16) & 0xFFFF;
        let minor = info.dw_file_version_ms & 0xFFFF;
        let build = (info.dw_file_version_ls >> 16) & 0xFFFF;
        let private = info.dw_file_version_ls & 0xFFFF;

        // Reconstruct Sunshine version string
        // The actual version "2025.924.154138" can't fit standard 16-bit
        // fields, so try the string table first.
        if let Ok(ver) = query_version_string(&buf) {
            return Ok(ver);
        }

        // Fallback to numeric version
        if build == 0 && private == 0 {
            Ok(format!("{}.{}", major, minor))
        } else {
            Ok(format!("{}.{}.{}.{}", major, minor, build, private))
        }
    }
}

/// Try to read the ProductVersion string from the version info string table.
fn query_version_string(version_info: &[u8]) -> Result<String> {
    use windows::Win32::Storage::FileSystem::VerQueryValueW;
    use windows::core::PCWSTR;

    // Common language/codepage combinations
    let lang_codepages = [
        "040904B0", // US English, Unicode
        "040904E4", // US English, Western European
        "000004B0", // Language Neutral, Unicode
    ];

    unsafe {
        for lc in &lang_codepages {
            let query = format!("\\StringFileInfo\\{}\\ProductVersion", lc);
            let query_wide: Vec<u16> = query.encode_utf16().chain(std::iter::once(0)).collect();

            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mut len = 0u32;

            if VerQueryValueW(
                version_info.as_ptr() as *const c_void,
                PCWSTR(query_wide.as_ptr()),
                &mut ptr,
                &mut len,
            )
            .as_bool()
                && !ptr.is_null()
                && len > 0
            {
                let slice = std::slice::from_raw_parts(ptr as *const u16, len as usize);
                let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                let version = String::from_utf16_lossy(&slice[..end]);
                let version = version.trim().to_string();
                if !version.is_empty() {
                    return Ok(version);
                }
            }
        }
    }

    Err(anyhow!("ProductVersion string not found"))
}

// ── PDB download & parsing ─────────────────────────────────────────────────────

/// Directory where extracted PDB files are cached.
fn pdb_cache_dir() -> PathBuf {
    std::env::temp_dir()
        .join("deragabu-agent")
        .join("sunshine-pdb")
}

/// Download the debuginfo archive and parse the PDB for the `display_cursor`
/// symbol, returning its RVA (Relative Virtual Address).
async fn download_and_parse_pdb(version: &str) -> Result<u32> {
    let cache = pdb_cache_dir();
    let pdb_path = cache.join(format!("sunshine-v{}.pdb", version));

    // If PDB is already cached, just parse it
    if pdb_path.exists() {
        info!("Using cached PDB: {:?}", pdb_path);
        return find_display_cursor_rva(&pdb_path);
    }

    // Download the .7z archive
    let url = DEBUGINFO_URL_TEMPLATE.replace("{VERSION}", version);
    info!("Downloading Sunshine debuginfo from: {}", url);

    let seven_z_path = cache.join(format!("debuginfo-v{}.7z", version));
    std::fs::create_dir_all(&cache)
        .context("Failed to create PDB cache directory")?;

    download_file(&url, &seven_z_path)
        .await
        .context("Failed to download debuginfo archive")?;

    info!("Downloaded to {:?}, extracting…", seven_z_path);

    // Extract .7z
    let extract_dir = cache.join(format!("extract-v{}", version));
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir).ok();
    }
    std::fs::create_dir_all(&extract_dir)?;

    // Run extraction in a blocking task
    let seven_z_clone = seven_z_path.clone();
    let extract_clone = extract_dir.clone();
    tokio::task::spawn_blocking(move || {
        sevenz_rust::decompress_file(&seven_z_clone, &extract_clone)
            .map_err(|e| anyhow!("7z extraction failed: {}", e))
    })
    .await
    .context("7z extraction task panicked")??;

    // Find the .pdb file in extracted content
    let found_pdb = find_pdb_file(&extract_dir)?;
    info!("Found PDB: {:?}", found_pdb);

    // Copy to cache location
    std::fs::copy(&found_pdb, &pdb_path)
        .context("Failed to cache PDB file")?;

    // Clean up extraction dir and archive
    std::fs::remove_dir_all(&extract_dir).ok();
    std::fs::remove_file(&seven_z_path).ok();

    find_display_cursor_rva(&pdb_path)
}

/// Download a file from a URL to a local path.
async fn download_file(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("Failed to create HTTP client")?;

    let response = client
        .get(url)
        .send()
        .await
        .context("HTTP request failed")?
        .error_for_status()
        .context("HTTP error response")?;

    let bytes = response
        .bytes()
        .await
        .context("Failed to read response body")?;

    std::fs::write(dest, &bytes).context("Failed to write file")?;

    info!("Downloaded {} bytes to {:?}", bytes.len(), dest);
    Ok(())
}

/// Recursively search for a .pdb file in a directory.
fn find_pdb_file(dir: &Path) -> Result<PathBuf> {
    for entry in walkdir(dir)? {
        if entry
            .extension()
            .map_or(false, |e| e.eq_ignore_ascii_case("pdb"))
        {
            return Ok(entry);
        }
    }
    Err(anyhow!("No .pdb file found in extracted archive"))
}

/// Simple recursive directory listing.
fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                results.extend(walkdir(&path)?);
            } else {
                results.push(path);
            }
        }
    }
    Ok(results)
}

/// Parse a PDB file and find the RVA of the `display_cursor` global variable.
fn find_display_cursor_rva(pdb_path: &Path) -> Result<u32> {
    use pdb::FallibleIterator;

    let file = std::fs::File::open(pdb_path)
        .with_context(|| format!("Cannot open PDB: {:?}", pdb_path))?;
    let mut pdb = pdb::PDB::open(file).context("Failed to parse PDB")?;

    let symbol_table = pdb.global_symbols().context("No global symbol table")?;
    let address_map = pdb.address_map().context("No address map in PDB")?;

    let mut iter = symbol_table.iter();
    while let Some(symbol) = iter.next().context("Error iterating PDB symbols")? {
        if let Ok(pdb::SymbolData::Data(data)) = symbol.parse() {
            let name = data.name.to_string();
            // Match both decorated and undecorated names
            if name == "display_cursor" || name.contains("display_cursor") {
                if let Some(rva) = data.offset.to_rva(&address_map) {
                    info!(
                        "PDB symbol '{}' found at RVA 0x{:08x}",
                        name,
                        rva.0
                    );
                    return Ok(rva.0);
                }
            }
        }
    }

    Err(anyhow!(
        "Symbol 'display_cursor' not found in PDB {:?}",
        pdb_path
    ))
}

// ── Process memory reading ─────────────────────────────────────────────────────

/// Read a single `bool` (1 byte) from a remote process at the given address.
fn read_process_bool(pid: u32, address: usize) -> Result<bool> {
    unsafe {
        let handle = OpenProcess(PROCESS_VM_READ, false, pid)
            .context("OpenProcess failed (need admin?)")?;
        let _guard = SafeHandle(handle);

        let mut value: u8 = 0;
        let mut bytes_read: usize = 0;

        ReadProcessMemory(
            handle,
            address as *const c_void,
            &mut value as *mut u8 as *mut c_void,
            1,
            Some(&mut bytes_read),
        )
        .context("ReadProcessMemory failed")?;

        if bytes_read != 1 {
            return Err(anyhow!("ReadProcessMemory: expected 1 byte, got {}", bytes_read));
        }

        Ok(value != 0)
    }
}

// ── Main monitor entry point ───────────────────────────────────────────────────

/// Start the Sunshine monitor.
///
/// 1. Locate the running `sunshine.exe` process.
/// 2. Detect its version and download the matching PDB with debug symbols.
/// 3. Parse the PDB to find the `display_cursor` global variable's RVA.
/// 4. Periodically read the live value from process memory.
/// 5. Emit [`SunshineSettingsEvent`] whenever the value changes.
pub async fn run_monitor(tx: mpsc::Sender<SunshineSettingsEvent>) -> Result<()> {
    info!("Sunshine monitor starting…");

    // ── Phase 1: find the running Sunshine process ──────────────────────────
    let proc = loop {
        match find_sunshine_process() {
            Ok(Some(p)) if p.module_base != 0 => break p,
            Ok(Some(p)) => {
                warn!(
                    "Found Sunshine (PID {}) but cannot read module base (access denied?)",
                    p.pid
                );
                tokio::time::sleep(Duration::from_secs(RETRY_INTERVAL_SECS)).await;
            }
            Ok(None) => {
                debug!("Sunshine process not found, retrying…");
                tokio::time::sleep(Duration::from_secs(RETRY_INTERVAL_SECS)).await;
            }
            Err(e) => {
                warn!("Error locating Sunshine process: {}", e);
                tokio::time::sleep(Duration::from_secs(RETRY_INTERVAL_SECS)).await;
            }
        }
    };

    info!(
        "Sunshine process found: PID={}, base=0x{:016x}, path={:?}",
        proc.pid, proc.module_base, proc.exe_path
    );

    // ── Phase 2: detect version ─────────────────────────────────────────────
    let version = detect_sunshine_version(&proc.exe_path);
    info!("Sunshine version: {}", version);

    // ── Phase 3: download PDB and find display_cursor RVA ───────────────────
    let rva = match download_and_parse_pdb(&version).await {
        Ok(rva) => {
            info!("display_cursor RVA: 0x{:08x}", rva);
            rva
        }
        Err(e) => {
            error!("Failed to resolve display_cursor from PDB: {}", e);
            // Cannot monitor without the symbol offset. Send default (true)
            // and keep the task alive so it doesn't crash the agent.
            let _ = tx
                .send(SunshineSettingsEvent { draw_cursor: true })
                .await;
            warn!("Sunshine monitor running in fallback mode (draw_cursor=true)");
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
    };

    let target_addr = proc.module_base + rva as usize;
    info!(
        "Will read display_cursor at 0x{:016x} (base 0x{:016x} + RVA 0x{:08x})",
        target_addr, proc.module_base, rva
    );

    // ── Phase 4: poll loop ──────────────────────────────────────────────────
    let mut last_value: Option<bool> = None;
    let mut consecutive_fails = 0u32;
    let mut poll = interval(Duration::from_millis(POLL_INTERVAL_MS));

    loop {
        poll.tick().await;

        match read_process_bool(proc.pid, target_addr) {
            Ok(val) => {
                consecutive_fails = 0;
                if last_value != Some(val) {
                    info!(
                        "Sunshine display_cursor: {:?} → {}",
                        last_value.map(|v| v.to_string()).unwrap_or("(init)".into()),
                        val
                    );
                    last_value = Some(val);
                    if tx
                        .send(SunshineSettingsEvent { draw_cursor: val })
                        .await
                        .is_err()
                    {
                        info!("Settings receiver dropped, stopping Sunshine monitor");
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                consecutive_fails += 1;
                if consecutive_fails <= 3 {
                    debug!("ReadProcessMemory failed (attempt {}): {}", consecutive_fails, e);
                } else if consecutive_fails == 4 {
                    warn!(
                        "Sunshine process may have exited (consecutive read failures: {})",
                        consecutive_fails
                    );
                }
                // After many failures, the process likely exited.
                // TODO: re-discover the process and re-attach.
                if consecutive_fails > 30 {
                    warn!("Too many consecutive read failures, restarting discovery…");
                    // Recurse into run_monitor to restart the whole flow
                    return Box::pin(run_monitor(tx)).await;
                }
            }
        }
    }
}
