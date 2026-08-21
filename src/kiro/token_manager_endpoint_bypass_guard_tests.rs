    //! 源码级守卫：**不经 provider** 的上游调用路径（深度验活 / 模型探测）必须走端点抽象，
    //! 不得手搓 IDE 的 host 与 profileArn 注入。
    //!
    //! 为何用源码断言：这两个函数都需要真实上游 + 号池才能跑通，纯单测覆盖不到；而它们正是
    //! CLI(ksk_)号的**自伤点** —— 硬编码 `runtime.{region}.kiro.dev` 会让 ksk_ 号稳定 403，
    //! 两者又都把 403/401 当"认证/账号级问题"上报：验活侧经 classify_balance_error
    //! **自动禁用凭据**，探测侧整轮中止并向面板报账号有问题。历史上这里已漏迁过一次 host
    //! （q.* → runtime.*），故把"必须走抽象"这条钉死。

    /// 取源码中某函数体的近似切片：从函数签名到下一个同缩进 `    }` 之前。
    fn fn_body(src: &str, signature: &str) -> String {
        let after = src
            .split(signature)
            .nth(1)
            .unwrap_or_else(|| panic!("函数 {signature} 不应被改名/删除"));
        // 4 空格缩进的收尾花括号即函数结束（本文件所有方法都在 impl 内，缩进固定）。
        after.split("\n    }").next().unwrap_or(after).to_string()
    }

    #[test]
    fn should_use_endpoint_abstraction_in_deep_verify() {
        let body = fn_body(
            include_str!("token_manager.rs"),
            "pub async fn deep_verify_credential(&self, id: u64)",
        );
        assert!(
            body.contains("endpoint::for_credentials"),
            "深度验活必须按凭据解析端点，否则 CLI 号 403 → 被自动禁用"
        );
        assert!(
            body.contains("endpoint.api_url(&rctx)"),
            "URL 必须来自端点实现"
        );
        assert!(
            !body.contains("runtime.{}.kiro.dev"),
            "不得硬编码 IDE host（CLI 号必须打 q.{{region}}.amazonaws.com）"
        );
        assert!(
            !body.contains("effective_profile_arn"),
            "profileArn 注入必须交给端点的 transform_api_body（CLI 带 ARN 会 403）"
        );
    }

    #[test]
    fn should_use_endpoint_abstraction_in_model_probe() {
        let body = fn_body(
            include_str!("token_manager.rs"),
            "async fn probe_single_model(",
        );
        assert!(
            body.contains("endpoint::for_credentials"),
            "模型探测必须按凭据解析端点，否则 CLI 号每个模型都 401/403 → 整轮中止"
        );
        assert!(
            body.contains("endpoint.api_url(&rctx)"),
            "URL 必须来自端点实现"
        );
        assert!(!body.contains("runtime.{}.kiro.dev"), "不得硬编码 IDE host");
        assert!(
            !body.contains("effective_profile_arn"),
            "profileArn 注入必须交给端点的 transform_api_body"
        );
    }
