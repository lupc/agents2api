fn main() {
    // Windows 下嵌入版本信息资源：无版本信息的无签名 exe 更容易被杀软启发式误报
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set("FileDescription", "CodeBuddy/WorkBuddy to OpenAI/Anthropic compatible local API converter");
        res.set("ProductName", "codebuddy2api-rs");
        res.set("OriginalFilename", "codebuddy2api.exe");
        res.set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            // 资源编译失败不应阻塞构建
            eprintln!("warning: winresource compile failed: {e}");
        }
    }
}
