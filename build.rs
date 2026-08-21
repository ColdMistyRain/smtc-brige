//! build.rs — 把版本/产品信息作为 VERSIONINFO 资源嵌入 Windows 可执行文件
//! （文件属性 → 详细信息 里显示的版本、产品名、版权等）。
//! 版本号自动取自 Cargo.toml 的 `version`，无需手动同步。
//! 非 Windows 目标直接跳过，不影响 Linux CI。

fn main() {
    // 仅在 Windows 目标生成资源脚本；Linux/其他平台无此需求。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // "0.1.1" -> (0, 1, 1, 0)，打包成 winres 期望的 u64（每段 16bit：
    // MAJOR << 48 | MINOR << 32 | PATCH << 16 | REV）
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let mut it = ver.split('.').map(|s| s.parse::<u16>().unwrap_or(0));
    let (major, minor, patch, rev) = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    let packed = (u64::from(major) << 48)
        | (u64::from(minor) << 32)
        | (u64::from(patch) << 16)
        | u64::from(rev);

    // WindowsResource::new() 已从 Cargo 环境变量填好 FileVersion/ProductVersion/
    // ProductName/FileDescription 等默认值，这里再显式覆盖为更友好的文案。
    let mut res = winres::WindowsResource::new();
    res.set_version_info(winres::VersionInfo::FILEVERSION, packed)
        .set_version_info(winres::VersionInfo::PRODUCTVERSION, packed)
        .set("FileVersion", &ver)
        .set("ProductVersion", &ver)
        .set("ProductName", "SMTC Bridge")
        .set("FileDescription", "SMTC / MPRIS media bridge: status, lyrics, cover and remote control for LAN devices")
        .set("OriginalFilename", "smtc-brige.exe")
        .set("CompanyName", "SMTC Bridge")
        .set("LegalCopyright", "Copyright (c) SMTC Bridge contributors")
        .set_language(0x0804); // 中文(简体，中国)，填充"语言"字段

    // rc.exe 缺失时只告警不中断（cargo check/clippy 也会跑 build.rs），
    // 真正的 cargo build 若缺 rc 本来就会在链接阶段失败。
    if let Err(e) = res.compile() {
        println!("cargo:warning=failed to embed Windows version resource: {e}");
    }
}
