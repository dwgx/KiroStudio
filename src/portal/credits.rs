//! 车队上车的计价规则（纯计算，零 I/O）。
//!
//! # 为什么独立成一个模块
//! 这里的公式是整套积分系统唯一的正确性核心。放在这一层意味着它能被**穷举验证**——
//! 不碰数据库、不起服务，直接跑 N=1..50 × 各种参数组合。若混进 `store.rs`，
//! 就只能靠集成测试覆盖，而集成测试跑不了几百种人数与参数的组合。
//!
//! # 两段式定价
//! ```text
//! N ≤ base_count → base_price                      （前几个人固定价）
//! N >  base_count → min(base_price, ceil(total/N))  （之后按人数均摊）
//! ```
//! 「几个人」和「多少分」是两个独立参数，所以「2 人 10 分」「4 人 5 分」「6 人 3 分」
//! 都能直接配出来，不必反推 total。
//!
//! # 后段那个 `min` 不是冗余
//! 它是防「人越多反而越贵」的唯一保障。反例：`base_count=4, base_price=5, total=100`，
//! 不取 min 时 N=5 的单价是 `ceil(100/5)=20`，比前 4 人的 5 分贵 4 倍——多来一个人
//! 全员涨价，激励彻底反了。取 min 后压平在 5 分。[`tests::pathological_config_still_monotonic`]
//! 锁住这条。

/// 计价参数。四个字段都可配，[`crate::model::config::Config`] 里有对应项。
///
/// 一把 key 首次被上车时，当时的参数会被冻结进 `portal_key_pricing` 表，
/// 此后这把 key 永远按快照计价——改配置只影响之后才首次被上车的 key。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pricing {
    /// 前几个人享受固定价。至少 1（0 会被 [`Self::sanitized`] 纠正）。
    pub base_count: u32,
    /// 前 `base_count` 个人的单价，同时是**价格上限**。
    pub base_price: i64,
    /// 定价基数。注意：**不是**「系统对这把 key 只收这么多」——
    /// ceil 取整会让实收超出它（N=9 时 9×3=27 > 20），见设计文档。
    pub total_price: i64,
    /// 单价下限。默认参数下永不触发（N 上限 10 时最低价是 ceil(20/10)=2 > 1）。
    pub min_price: i64,
    /// 每把 key 最多几人上车。满员后拒绝，不扣分。
    pub max_unlockers: u32,
}

impl Default for Pricing {
    fn default() -> Self {
        Pricing {
            base_count: 2,
            base_price: 10,
            total_price: 20,
            min_price: 1,
            max_unlockers: 10,
        }
    }
}

impl Pricing {
    /// 把不合理的参数纠正成安全值。
    ///
    /// 【为何必须有这一步】参数来自 config.json，用户可以手写任意值。`base_count=0`
    /// 会让「前几人固定价」这段消失、`min_price` 为负会让余额被加回去（负价 = 送分）、
    /// `max_unlockers=0` 会让任何人都上不了车。与其在每个调用点各判一遍，
    /// 不如在入口处一次性收敛——下游代码就可以假定参数总是合理的。
    pub fn sanitized(self) -> Self {
        Pricing {
            base_count: self.base_count.max(1),
            base_price: self.base_price.max(0),
            total_price: self.total_price.max(0),
            // 下限不能为负：负单价意味着「上车反而赚分」。
            min_price: self.min_price.max(0),
            max_unlockers: self.max_unlockers.max(1),
        }
    }

    /// 给定当前上车人数 `n`，算这把 key 现在的单价。
    ///
    /// `n` 传 0 按 1 处理（没人上车时的「下一个人要付多少」等于 N=1 的价）。
    pub fn unit_price(&self, n: u32) -> i64 {
        let p = self.sanitized();
        let n = n.max(1);

        let raw = if n <= p.base_count {
            p.base_price
        } else {
            // 整数 ceil：不走浮点。20/3 在 f64 里是 6.666…7，
            // 依赖它的舍入行为等于把计费正确性交给浮点表示。
            let shared = div_ceil(p.total_price, n as i64);
            // min 钳制：见模块级说明，防「人越多越贵」。
            shared.min(p.base_price)
        };

        raw.max(p.min_price)
    }

    /// 该 key 是否已满员。
    pub fn is_full(&self, n: u32) -> bool {
        n >= self.sanitized().max_unlockers
    }
}

/// 整数向上取整除法。`b <= 0` 时返回 `a`（退化保护，不 panic）。
///
/// 用 `(a + b - 1) / b` 而非 `(a as f64 / b as f64).ceil()`：
/// 后者在大数或除不尽时依赖浮点表示，而这里算的是钱。
fn div_ceil(a: i64, b: i64) -> i64 {
    if b <= 0 {
        return a;
    }
    if a <= 0 {
        return 0;
    }
    (a + b - 1) / b
}

/// 一个已上车用户的退款额：已付 − 当前应付。
///
/// # 差额模型
/// 系统只记「已付多少」，不记「退过多少」。每次人数变化时，用
/// `已付 − 应付` 重算一次退款。这样**任何时刻**每人的净支出都恒等于
/// `unit_price(N)`，总账自然自洽，不需要单独的对账逻辑。
///
/// 反面做法是「按当前单价退增量」：ceil 之下那种做法的误差会随人数变化累积，
/// 最终没人能解释某个用户为什么比别人多付 1 分。
///
/// 返回值恒 ≥ 0——已付少于应付时不追扣（那种情况只会在管理员改价后出现，
/// 而设计上改价只影响新 key，所以实际不会发生；这里仍然 clamp 住，
/// 避免将来某个改动让它变成静默扣款）。
pub fn refund_for(paid: i64, current_price: i64) -> i64 {
    (paid - current_price).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户给的两个例子。写死在测试里：这是需求的原话，
    /// 日后任何"优化"改动了这两条序列，就是改了需求。
    #[test]
    fn user_examples_match_exactly() {
        // 「2 人 10 分」
        let a = Pricing {
            base_count: 2,
            base_price: 10,
            total_price: 20,
            min_price: 1,
            max_unlockers: 10,
        };
        let got: Vec<i64> = (1..=10).map(|n| a.unit_price(n)).collect();
        assert_eq!(
            got,
            vec![10, 10, 7, 5, 4, 4, 3, 3, 3, 2],
            "「2 人 10 分」的价格序列变了"
        );

        // 「4 人 5 分」——前 4 个人都是 5 分
        let b = Pricing {
            base_count: 4,
            base_price: 5,
            total_price: 20,
            min_price: 1,
            max_unlockers: 10,
        };
        let got: Vec<i64> = (1..=10).map(|n| b.unit_price(n)).collect();
        assert_eq!(
            got,
            vec![5, 5, 5, 5, 4, 4, 3, 3, 3, 2],
            "「4 人 5 分」的价格序列变了"
        );
    }

    /// 价格必须随人数单调不增。
    ///
    /// 这是整套定价的**根本性质**：多来一个人，谁的价格都不该上涨。
    /// 违反它意味着老用户会因为新人加入而被追加扣费（或退款变成负数）。
    #[test]
    fn price_is_monotonically_non_increasing() {
        let configs = [
            Pricing::default(),
            Pricing {
                base_count: 1,
                base_price: 10,
                total_price: 20,
                min_price: 1,
                max_unlockers: 10,
            },
            Pricing {
                base_count: 4,
                base_price: 5,
                total_price: 20,
                min_price: 1,
                max_unlockers: 10,
            },
            Pricing {
                base_count: 6,
                base_price: 3,
                total_price: 20,
                min_price: 1,
                max_unlockers: 10,
            },
            Pricing {
                base_count: 10,
                base_price: 10,
                total_price: 20,
                min_price: 1,
                max_unlockers: 10,
            },
            Pricing {
                base_count: 3,
                base_price: 8,
                total_price: 10,
                min_price: 1,
                max_unlockers: 10,
            },
            Pricing {
                base_count: 2,
                base_price: 100,
                total_price: 1000,
                min_price: 5,
                max_unlockers: 50,
            },
        ];
        for p in configs {
            let mut prev = i64::MAX;
            for n in 1..=50u32 {
                let cur = p.unit_price(n);
                assert!(
                    cur <= prev,
                    "{p:?} 在 N={n} 时价格从 {prev} 涨到 {cur}——人越多越贵"
                );
                prev = cur;
            }
        }
    }

    /// 病态配置：`total` 远大于 `base_price` 时，后段若不取 min 会暴涨。
    ///
    /// 这正是 `min` 钳制存在的理由。不加 min 时该配置的序列是
    /// `5 5 5 5 20 17 15 …`——第 5 个人加入让单价翻 4 倍。
    #[test]
    fn pathological_config_still_monotonic() {
        let p = Pricing {
            base_count: 4,
            base_price: 5,
            total_price: 100, // 故意远大于 base_price
            min_price: 1,
            max_unlockers: 10,
        };
        let got: Vec<i64> = (1..=10).map(|n| p.unit_price(n)).collect();
        assert_eq!(
            got,
            vec![5, 5, 5, 5, 5, 5, 5, 5, 5, 5],
            "min 钳制失效，价格会随人数暴涨"
        );
    }

    /// 单价恒在 [min_price, base_price] 区间内。
    #[test]
    fn price_stays_within_bounds() {
        for base_count in 1..=6u32 {
            for base_price in [1i64, 5, 10, 100] {
                for total in [0i64, 1, 20, 50, 1000] {
                    for min in [0i64, 1, 3] {
                        let p = Pricing {
                            base_count,
                            base_price,
                            total_price: total,
                            min_price: min,
                            max_unlockers: 20,
                        };
                        for n in 1..=20u32 {
                            let v = p.unit_price(n);
                            assert!(v >= min, "{p:?} N={n} 单价 {v} 低于下限 {min}");
                            assert!(
                                v <= base_price.max(min),
                                "{p:?} N={n} 单价 {v} 超过上限 {base_price}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// 差额模型的核心不变量：**任何时刻**每个已上车用户的净支出都等于当前单价。
    ///
    /// 模拟 1→max 人陆续上车，每步之后把所有人的账都验一遍。
    /// 这是整套退款逻辑正确性的唯一证明——设计阶段用 Python 预演过，
    /// 这里用真实实现再跑一遍。
    #[test]
    fn differential_model_keeps_everyone_equal() {
        for p in [
            Pricing::default(),
            Pricing {
                base_count: 4,
                base_price: 5,
                total_price: 20,
                min_price: 1,
                max_unlockers: 10,
            },
            Pricing {
                base_count: 1,
                base_price: 20,
                total_price: 60,
                min_price: 2,
                max_unlockers: 30,
            },
        ] {
            // user_index -> (累计已付, 净支出)
            let mut paid: Vec<i64> = Vec::new();
            let mut net: Vec<i64> = Vec::new();

            for n in 1..=p.max_unlockers {
                let price = p.unit_price(n);

                // 新人上车：扣 price
                paid.push(price);
                net.push(price);

                // 老人退差额
                for i in 0..(paid.len() - 1) {
                    let r = refund_for(paid[i], price);
                    if r > 0 {
                        paid[i] -= r;
                        net[i] -= r;
                    }
                }

                // 不变量：所有人的已付与净支出都等于当前单价
                for i in 0..paid.len() {
                    assert_eq!(
                        paid[i], price,
                        "{p:?} N={n} 用户{i} 已付 {} != 当前单价 {price}",
                        paid[i]
                    );
                    assert_eq!(
                        net[i], price,
                        "{p:?} N={n} 用户{i} 净支出 {} != 当前单价 {price}",
                        net[i]
                    );
                }
            }
        }
    }

    /// 满员判定：达到上限后 `is_full` 为真。
    #[test]
    fn full_at_max_unlockers() {
        let p = Pricing::default(); // max=10
        assert!(!p.is_full(0));
        assert!(!p.is_full(9));
        assert!(p.is_full(10), "10 人应判满员");
        assert!(p.is_full(11), "超出也算满员（防御性）");

        let one = Pricing {
            max_unlockers: 1,
            ..Pricing::default()
        };
        assert!(!one.is_full(0));
        assert!(one.is_full(1), "上限 1 时第 2 人应被拒");
    }

    /// 参数为 0 或负数时不 panic、不产生负价。
    ///
    /// 配置来自用户手写的 JSON，`base_count=0`、`min_price=-5` 都可能出现。
    /// 负单价意味着「上车反而加分」，那是白送积分的漏洞。
    #[test]
    fn degenerate_params_are_sanitized() {
        let zero = Pricing {
            base_count: 0,
            base_price: 0,
            total_price: 0,
            min_price: 0,
            max_unlockers: 0,
        };
        for n in 0..=5u32 {
            assert_eq!(zero.unit_price(n), 0, "全 0 参数应得 0 价，不应 panic");
        }
        assert!(zero.is_full(1), "max_unlockers=0 被纠正为 1");

        let neg = Pricing {
            base_count: 0,
            base_price: -10,
            total_price: -20,
            min_price: -5,
            max_unlockers: 0,
        };
        for n in 0..=5u32 {
            let v = neg.unit_price(n);
            assert!(v >= 0, "N={n} 单价 {v} 为负——上车会给用户加分");
        }
    }

    /// `min_price` 高于算出来的均摊价时，下限生效。
    #[test]
    fn min_price_floors_the_result() {
        let p = Pricing {
            base_count: 1,
            base_price: 10,
            total_price: 20,
            min_price: 4, // 高于 N≥5 时的均摊价
            max_unlockers: 20,
        };
        assert_eq!(p.unit_price(1), 10);
        assert_eq!(p.unit_price(4), 5);
        assert_eq!(p.unit_price(10), 4, "ceil(20/10)=2 应被下限 4 抬起");
        assert_eq!(p.unit_price(20), 4, "下限持续生效");
    }

    /// 整数 ceil 的行为。
    #[test]
    fn div_ceil_is_integer_math() {
        assert_eq!(div_ceil(20, 3), 7, "20/3 向上取整应为 7");
        assert_eq!(div_ceil(20, 4), 5, "整除不应多加 1");
        assert_eq!(div_ceil(20, 10), 2);
        assert_eq!(div_ceil(1, 3), 1);
        assert_eq!(div_ceil(0, 3), 0);
        assert_eq!(div_ceil(-5, 3), 0, "负分子不产生负价");
        assert_eq!(div_ceil(20, 0), 20, "除 0 退化返回原值，不 panic");
        assert_eq!(div_ceil(20, -1), 20, "负除数退化返回原值");
        // 大数不溢出
        assert_eq!(div_ceil(i64::MAX / 2, i64::MAX / 2), 1);
    }

    /// 退款额恒非负——已付少于应付时不追扣。
    #[test]
    fn refund_never_negative() {
        assert_eq!(refund_for(7, 5), 2);
        assert_eq!(refund_for(5, 5), 0);
        assert_eq!(refund_for(3, 10), 0, "已付少于应付时不得追扣");
        assert_eq!(refund_for(0, 10), 0);
    }

    /// N=0 与 N=1 同价：「还没人上车时，下一个人要付多少」。
    #[test]
    fn zero_treated_as_one() {
        let p = Pricing::default();
        assert_eq!(p.unit_price(0), p.unit_price(1));
    }
}
