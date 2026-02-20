// ============================================================
// selfupdate.rs — 更新器自更新模块
// ============================================================
// 负责：
//   1. 计算当前 exe 的 SHA256
//   2. 对比远程 server.json 中的 updater_sha256
//   3. 如果不同，下载新 exe → 替换自身 → 重启
//   4. 清理旧版 exe 残留 (.old)
//
// Windows 上正在运行的 exe 不能直接覆盖，但可以重命名。
// 策略：旧 exe → rename .old → 新 exe 写入原路径 → 重启。
// ============================================================

use anyhow::{Context, Result};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config;

/// 自更新检查结果
pub enum SelfUpdateResult {
    /// 无需更新，继续正常流程
    UpToDate,
    /// 已下载新版并重启，调用方应立即退出
    Restarting,
}

/// 获取当前 exe 的路径
fn current_exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("无法获取当前 exe 路径")
}

/// 清理上次自更新留下的 .old 文件
pub fn cleanup_old_exe() {
    if let Ok(exe) = current_exe_path() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            // 可能上次更新后重启的，删掉旧版
            let _ = fs::remove_file(&old);
        }
    }
}

/// 计算文件的 SHA256 哈希值（小写十六进制）
fn sha256_file(path: &Path) -> Result<String> {
    use std::io::BufReader;

    let file = fs::File::open(path)
        .with_context(|| format!("打开文件失败: {}", path.display()))?;
    let mut reader = BufReader::new(file);

    // 手动实现 SHA256（避免额外依赖）
    // 使用 Windows CryptoAPI
    sha256_digest(&mut reader)
}

/// 使用纯 Rust 实现的 SHA256
/// 为避免添加额外 crate，使用简单的 SHA256 实现
fn sha256_digest<R: Read>(reader: &mut R) -> Result<String> {
    // 使用标准的 SHA256 常量和算法
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf).context("读取文件失败")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
}

// ── 内置 SHA256 实现 ──

struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    total_len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
    0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buffer: Vec::new(),
            total_len: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.total_len += data.len() as u64;
        self.buffer.extend_from_slice(data);

        while self.buffer.len() >= 64 {
            let block: [u8; 64] = self.buffer[..64].try_into().unwrap();
            self.buffer.drain(..64);
            self.process_block(&block);
        }
    }

    fn process_block(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }

    fn finalize_hex(mut self) -> String {
        // Padding
        let bit_len = self.total_len * 8;
        self.buffer.push(0x80);
        while (self.buffer.len() % 64) != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_len.to_be_bytes());

        // Process remaining blocks
        let remaining = self.buffer.clone();
        for chunk in remaining.chunks_exact(64) {
            let block: [u8; 64] = chunk.try_into().unwrap();
            self.process_block(&block);
        }

        // Output
        self.state
            .iter()
            .map(|v| format!("{:08x}", v))
            .collect::<String>()
    }
}

/// 检查并执行自更新。
///
/// 返回 `SelfUpdateResult::Restarting` 时，调用方应立即退出进程。
pub fn check_and_update(
    updater_url: Option<&str>,
    updater_sha256: Option<&str>,
    on_progress: &dyn Fn(crate::update::Progress),
) -> Result<SelfUpdateResult> {
    // 如果没有配置自更新 URL 或哈希，跳过
    let (url, expected_hash) = match (updater_url, updater_sha256) {
        (Some(u), Some(h)) => (u, h),
        _ => return Ok(SelfUpdateResult::UpToDate),
    };

    let exe_path = current_exe_path()?;

    on_progress(crate::update::Progress::new(1, "检查更新器版本..."));

    // 计算当前 exe 的哈希
    let current_hash = sha256_file(&exe_path)?;

    if current_hash == expected_hash {
        return Ok(SelfUpdateResult::UpToDate);
    }

    on_progress(crate::update::Progress::new(2, "发现更新器新版本，正在下载..."));

    // 下载新 exe 到临时文件
    let temp_path = exe_path.with_extension("exe.new");

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(config::DOWNLOAD_TIMEOUT_SECS))
        .build();

    let response = agent
        .get(url)
        .call()
        .context("下载更新器新版本失败")?;

    // 获取文件大小
    let total_size = response
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let mut reader = response.into_reader();
    let mut file = fs::File::create(&temp_path)
        .context("创建临时文件失败")?;

    let mut buf = [0u8; 65536];
    let mut downloaded: u64 = 0;
    {
        use std::io::Write;
        loop {
            let n = reader.read(&mut buf).context("读取下载数据失败")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).context("写入文件失败")?;
            downloaded += n as u64;

            if total_size > 0 {
                let fraction = downloaded as f64 / total_size as f64;
                let pct = 2 + (fraction * 8.0) as u32; // 2% ~ 10%
                let mb_done = downloaded as f64 / 1_048_576.0;
                let mb_total = total_size as f64 / 1_048_576.0;
                on_progress(crate::update::Progress::new(
                    pct.min(10),
                    format!("下载更新器... {:.1}/{:.1} MB", mb_done, mb_total),
                ));
            }
        }
    }
    drop(file);

    // 验证下载的文件哈希
    let new_hash = sha256_file(&temp_path)?;
    if new_hash != expected_hash {
        let _ = fs::remove_file(&temp_path);
        anyhow::bail!(
            "更新器下载校验失败\n\
             预期: {}\n\
             实际: {}",
            expected_hash,
            new_hash
        );
    }

    on_progress(crate::update::Progress::new(10, "正在替换更新器..."));

    // 替换流程：旧 exe → .old，新 exe → 原路径
    let old_path = exe_path.with_extension("exe.old");

    // 删除可能残留的旧 .old
    if old_path.exists() {
        fs::remove_file(&old_path).ok();
    }

    // 重命名当前运行的 exe（Windows 允许重命名正在运行的 exe）
    fs::rename(&exe_path, &old_path)
        .context("重命名旧版更新器失败")?;

    // 移动新 exe 到原路径
    if let Err(e) = fs::rename(&temp_path, &exe_path) {
        // 回滚：把旧的移回去
        let _ = fs::rename(&old_path, &exe_path);
        return Err(e).context("替换更新器失败");
    }

    on_progress(crate::update::Progress::new(11, "更新器已更新，正在重启..."));

    // 启动新版 exe
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    std::process::Command::new(&exe_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("启动新版更新器失败")?;

    Ok(SelfUpdateResult::Restarting)
}
