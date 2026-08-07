//! 带**体积上限**的 HTTP 响应体读取。
//!
//! # 为什么需要这个模块
//!
//! `reqwest` 的 `resp.bytes()` / `resp.json()` 会把**整个**响应体读进内存且**无上限**。
//! 对任何指向外部（尤其第三方）端点的请求，这等于把内存占用交给对端决定：
//! 一个被劫持或投毒的上游返回 10GB chunked 响应，就能把**正在服务的网关进程** OOM 掉。
//!
//! `Content-Length` 预检**不够**：`Transfer-Encoding: chunked` 的响应没有该头，
//! 预检整个被跳过。所以必须**流式累计并在超限处中断**。
//!
//! # 为什么收口成公共模块
//!
//! 仓库里此前有两份各自实现的截断逻辑（`admin_ui::router` 的背景图图片跳、
//! `admin::update` 的 OTA 二进制下载），而**同一文件里的 JSON 那一跳被漏掉了** ——
//! 正是"每处各写一份"导致的漏改。收口成一处后，新增调用点只需调用，不必重新想一遍。

use anyhow::Result;

/// 流式读取响应体，累计超过 `cap` 字节即中断并返回 `Err`。
///
/// `what` 只用于错误信息，便于运维定位是哪一跳超限。
///
/// # 为什么先做 Content-Length 预检
///
/// 有该头时能在**下载之前**就拒绝，省掉整次传输；缺失（chunked）时靠下面的累计兜底。
/// 两道都要，缺任一道都有绕过路径。
pub async fn read_body_capped(resp: reqwest::Response, what: &str, cap: u64) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > cap {
            anyhow::bail!("{what} Content-Length {len} 超过上限 {cap}，拒绝读取");
        }
    }
    let mut resp = resp;
    // 预分配按 Content-Length（已确认 ≤ cap）；缺失时从 0 起，
    // 避免对 chunked 响应按 cap 预留一大块内存（那本身就是一种放大）。
    let mut buf: Vec<u8> = Vec::with_capacity(resp.content_length().unwrap_or(0) as usize);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("{what} 读取响应体失败: {e}"))?
    {
        // 这道是 chunked（无 Content-Length）场景下**唯一**的防线。
        // 判定放在 extend 之前，故 cap 是真上限而非"超出一个 chunk 后才发现"。
        if buf.len() as u64 + chunk.len() as u64 > cap {
            anyhow::bail!(
                "{what} 响应体超过上限 {cap}（已读 {} 字节即中断；\
                 常见于上游用 chunked 编码绕过 Content-Length 预检）",
                buf.len()
            );
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// 带上限地读取并反序列化 JSON。
///
/// 等价于"先 [`read_body_capped`] 再 `serde_json::from_slice`"，
/// 但把这个常见组合固定下来，避免调用方图省事又写回 `resp.json()`。
pub async fn read_json_capped<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    what: &str,
    cap: u64,
) -> Result<T> {
    let bytes = read_body_capped(resp, what, cap).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("{what} JSON 解析失败（已读 {} 字节）: {e}", bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// 起一个**只发 chunked、不带 Content-Length** 的测试服务端，推 `chunks` 个 64KiB 块。
    async fn rogue_chunked_server(chunks: usize) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // 必须把请求头读到空行为止再回响应，否则 hyper 侧会报 IncompleteMessage
            // （表现为"连接坏了"，与被测逻辑无关）。
            let mut req = Vec::new();
            let mut b = [0u8; 1024];
            loop {
                match tokio::io::AsyncReadExt::read(&mut sock, &mut b).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        req.extend_from_slice(&b[..n]);
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            if sock
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            let payload = vec![b'A'; 64 * 1024];
            let head = format!("{:x}\r\n", payload.len());
            for _ in 0..chunks {
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                if sock.write_all(&payload).await.is_err() {
                    return;
                }
                if sock.write_all(b"\r\n").await.is_err() {
                    return;
                }
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.flush().await;
        });
        port
    }

    /// ⚠️ 必须 `no_proxy()`：本 crate 的 reqwest 开了 `system-proxy` feature，
    /// 默认读系统代理。开发机装了 Clash/Surge 时会尝试把到 127.0.0.1 的请求塞进代理隧道
    /// → 握手失败，报 hyper IncompleteMessage（与被测逻辑无关）。
    /// 这与已知问题 #19 同型：测试必须与本机网络环境无关。
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("构造测试 client")
    }

    /// 核心回归：**chunked 响应（无 Content-Length）也必须受上限约束**。
    ///
    /// 这正是 `resp.bytes()` / `resp.json()` 的绕过路径：只做 Content-Length 预检时，
    /// chunked 响应会整个跳过检查、无上限读进内存 → OOM 掉正在服务的网关进程。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chunked_body_without_content_length_is_capped() {
        let port = rogue_chunked_server(16).await; // 推 1MiB
        let resp = test_client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("请求应成功建立");
        assert!(
            resp.content_length().is_none(),
            "前提：chunked 响应不应带 Content-Length，否则测不到该绕过路径"
        );

        const CAP: u64 = 64 * 1024;
        let err = read_body_capped(resp, "test-chunked", CAP)
            .await
            .expect_err("超限的 chunked 响应必须返回 Err（无上限实现会读完 1MiB 后返 Ok）");
        assert!(
            err.to_string().contains("超过上限"),
            "错误应明确指出超限，便于运维定位: {err}"
        );
    }

    /// 未超限的响应应正常读完（对照组：防止上限做得过严把正常响应也拒了）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn body_under_cap_reads_fully() {
        let port = rogue_chunked_server(1).await; // 64KiB
        let resp = test_client()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("请求应成功建立");
        let body = read_body_capped(resp, "test-ok", 1024 * 1024)
            .await
            .expect("未超限应正常读完");
        assert_eq!(body.len(), 64 * 1024);
    }
}
