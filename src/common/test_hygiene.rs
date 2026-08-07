//! 测试卫生守卫 —— 防「孤立测试」整类问题。
//!
//! # 它守的是什么
//!
//! 本仓的测试全是各源文件内联的 `#[cfg(test)]` 模块。若某个测试函数的 `#[test]` 属性丢失
//! （最常见成因：上一条测试的**文档注释块**把属性行吃掉，或重构时误删），那条测试就
//! **静默停止运行**。
//!
//! 失效形态是编译期 `function is never used` 警告 —— **不是测试失败** ——
//! 而本仓编译输出常有十几条同类警告，所以没人会注意到其中一条是死掉的守卫。
//!
//! # 为什么值得一条专门的守卫（而不是每次手工扫）
//!
//! 实测这个问题**已经复发至少三次**：
//! - 2026-08-06 之前某轮：全仓扫出 3 处，其中 2 处是**从未运行过**的真测试
//!   （`service.rs` 的 `multi_open_must_inherit_api_region_from_parent`、
//!   `provider.rs` 的 `force_refresh_must_skip_api_key_credentials_at_both_sites`），
//!   补属性后**两条一次通过** —— 说明它们守的东西一直对，只是守卫没生效。
//! - 同一个 `force_refresh_must_skip_api_key_credentials_at_both_sites` **又退化了一次**
//!   （2026-08-06 再次发现缺属性），所以「修一次」显然不够，需要机器来守。
//!
//! 单修一个实例只解决当次；这条守卫让整类问题在 CI 就暴露。
//!
//! # 判据（刻意保守，宁可漏报不误报）
//!
//! 命中条件**全部满足**才报：
//! 1. 在 `#[cfg(test)]` 之后（只看测试段，生产代码不管）
//! 2. `fn 名字()` —— **无参数、无返回值**（测试函数的形状；带参数的是辅助函数）
//! 3. 函数体内含 `assert`（纯 setup 辅助函数没有断言，排除掉）
//! 4. 紧邻上方**没有** `#[test]` / `#[tokio::test]` / `#[allow(dead_code)]`
//!
//! 第 3 条是防误报的关键：`fn make_cred()` 这类无参无返回的构造辅助会被第 2 条命中，
//! 但它没有 `assert` ⇒ 不报。若确有「无参无返回且含 assert」的辅助函数，
//! 给它加 `#[allow(dead_code)]` 即可豁免（那也正好是它本该有的属性）。

/// 一条疑似孤立测试的定位。
#[derive(Debug)]
pub struct OrphanTest {
    pub file: String,
    pub line: usize,
    pub name: String,
}

/// 扫描一份源码文本，找出疑似孤立测试。判据见模块文档。
///
/// 纯函数（吃字符串不碰文件系统），便于用固定夹具做「回退即 FAIL」验证。
pub fn find_orphan_tests(file: &str, src: &str) -> Vec<OrphanTest> {
    let lines: Vec<&str> = src.lines().collect();
    // 只看 `#[cfg(test)]` 之后的部分。找不到就整份跳过（该文件没有测试段）。
    let start = match lines.iter().position(|l| l.contains("#[cfg(test)]")) {
        Some(i) => i,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for (idx, raw) in lines.iter().enumerate().skip(start) {
        let line = raw.trim();
        // 判据 2：`fn 名字()` 无参无返回。允许 `async fn`。
        // 不匹配 `fn f(x: T)`（有参）、`fn f() -> T`（有返回）。
        let after_fn = match line
            .strip_prefix("fn ")
            .or_else(|| line.strip_prefix("async fn "))
            .or_else(|| line.strip_prefix("pub fn "))
        {
            Some(rest) => rest,
            None => continue,
        };
        let Some(paren) = after_fn.find('(') else {
            continue;
        };
        let name = after_fn[..paren].trim();
        if name.is_empty() || !after_fn[paren..].starts_with("()") {
            continue; // 有参数
        }
        let tail = after_fn[paren + 2..].trim();
        if !tail.starts_with('{') {
            continue; // 有返回值（`-> T {`）或其它形状
        }

        // 判据 4：紧邻上方是否有 test / 豁免属性。向上跳过空行、注释、其它属性。
        let mut has_attr = false;
        let mut k = idx;
        while k > 0 {
            k -= 1;
            let prev = lines[k].trim();
            if prev.is_empty() || prev.starts_with("//") {
                continue;
            }
            if prev.starts_with("#[") || prev.starts_with("#!") {
                if prev.contains("test")
                    || prev.contains("allow(dead_code)")
                    || prev.contains("ignore")
                {
                    has_attr = true;
                    break;
                }
                continue; // 其它属性（如 #[rustfmt::skip]）继续往上找
            }
            break; // 遇到实代码行，停
        }
        if has_attr {
            continue;
        }

        // 判据 3：函数体内含 `assert`。按大括号配平找函数体结束。
        // ⚠️ 只作启发式：字符串里的大括号会算进来，但对「体内有没有 assert」这个
        // 判断几乎无影响（最坏是多扫几行，仍然只影响是否报告）。
        let mut depth = 0usize;
        let mut has_assert = false;
        for probe in lines.iter().skip(idx) {
            depth += probe.matches('{').count();
            depth = depth.saturating_sub(probe.matches('}').count());
            if probe.contains("assert") {
                has_assert = true;
            }
            if depth == 0 {
                break;
            }
        }
        if !has_assert {
            continue;
        }

        out.push(OrphanTest {
            file: file.to_string(),
            line: idx + 1,
            name: name.to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 全仓扫描：任何疑似孤立测试都让 CI 红。
    ///
    /// 用 `std::fs` 递归读 `src/`（`include_str!` 不支持通配符，而硬编码文件名单
    /// 本身就是「两份手工名单会漂」的同类缺陷 —— 新文件加了测试却忘了加进名单，
    /// 守卫就对它无效）。cargo test 的工作目录是 crate 根，故相对路径 `src` 可用。
    #[test]
    fn no_orphan_tests_in_repo() {
        let mut found: Vec<OrphanTest> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from("src")];
        let mut scanned = 0usize;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    // `test.rs` / `debug.rs` 是仓里已知的**孤儿文件**（`main.rs` 的 mod
                    // 列表里没有它们，且已与现有 API 脱节），不参与编译也不该被扫。
                    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    if stem == "test" || stem == "debug" {
                        continue;
                    }
                    // 🔴 跳过**本文件自己**。它的判据测试用「看起来像孤立测试的源码」
                    // 作字符串夹具，而扫描器读的是原始文本 —— 分辨不出那是字面量还是真代码
                    // （要分辨得写真 Rust 解析器，代价远超收益）。
                    // 实测：不跳过时本守卫会稳定报 2 处自身夹具误报 ⇒ 永久红 ⇒
                    // 下一个人会直接把整条守卫删掉，那比漏扫一个文件糟得多。
                    // 代价是本文件内的真孤立测试扫不到；由上面三条判据测试（正例+反例）
                    // 兜住 —— 它们本身就是本文件的 #[test]，属性丢了会立刻表现为覆盖缺失。
                    if stem == "test_hygiene" {
                        continue;
                    }
                    if let Ok(src) = std::fs::read_to_string(&p) {
                        scanned += 1;
                        found.extend(find_orphan_tests(&p.display().to_string(), &src));
                    }
                }
            }
        }

        // 自检：扫不到文件说明路径/工作目录假设错了 —— 那时"零命中"是假的绿。
        // 这正是本仓记录的「纸面测试」形态之一（夹具不含被判据匹配的内容，恒绿）。
        assert!(
            scanned > 20,
            "只扫到 {scanned} 个 .rs 文件，工作目录假设可能不成立 ⇒ 本守卫会假绿。\
             cargo test 的 cwd 应为 crate 根。"
        );

        assert!(
            found.is_empty(),
            "发现 {} 处疑似**孤立测试**（无参无返回、体内有 assert、却没有 #[test] 属性）\
             ⇒ 它们**从未运行过**，且失效形态是编译警告而非测试失败，所以不会有人注意：\n{}\n\
             修法：补 `#[test]`（若是真测试）或加 `#[allow(dead_code)]`（若是辅助函数）。",
            found.len(),
            found
                .iter()
                .map(|o| format!("  {}:{} fn {}()", o.file, o.line, o.name))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// 判据本身的正例：缺属性 + 有 assert ⇒ 必须命中。
    #[test]
    fn detects_missing_test_attribute() {
        let src = "\
#[cfg(test)]
mod tests {
    fn some_guard_that_never_runs() {
        assert!(true);
    }
}
";
        let hits = find_orphan_tests("fixture.rs", src);
        assert_eq!(hits.len(), 1, "缺 #[test] 且含 assert 必须命中：{hits:?}");
        assert_eq!(hits[0].name, "some_guard_that_never_runs");
    }

    /// 反例三组：有属性 / 无 assert 的辅助 / 带参数的辅助 —— 都不该报。
    /// 这三条是防误报的护栏：误报会让人直接把守卫关掉，比漏报更糟。
    #[test]
    fn does_not_flag_legit_shapes() {
        let src = "\
#[cfg(test)]
mod tests {
    #[test]
    fn real_test() {
        assert!(true);
    }

    fn make_fixture() {
        let _x = 1;
    }

    fn helper_with_args(n: usize) {
        assert!(n > 0);
    }

    #[allow(dead_code)]
    fn exempted_helper() {
        assert!(true);
    }
}
";
        let hits = find_orphan_tests("fixture.rs", src);
        assert!(hits.is_empty(), "不该报这些形状：{hits:?}");
    }

    /// 生产段的函数不该被扫（判据 1：只看 `#[cfg(test)]` 之后）。
    #[test]
    fn ignores_production_functions() {
        let src = "\
fn prod_fn() {
    assert!(true);
}
";
        assert!(find_orphan_tests("fixture.rs", src).is_empty());
    }
}
