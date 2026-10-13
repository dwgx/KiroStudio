//! AWS Event Stream 头部解析
//!
//! 实现 AWS Event Stream 协议的头部解析功能

use super::error::{ParseError, ParseResult};
use std::collections::HashMap;

/// 头部值类型标识
///
/// AWS Event Stream 协议定义的 10 种值类型
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderValueType {
    BoolTrue = 0,
    BoolFalse = 1,
    Byte = 2,
    Short = 3,
    Integer = 4,
    Long = 5,
    ByteArray = 6,
    String = 7,
    Timestamp = 8,
    Uuid = 9,
}

impl TryFrom<u8> for HeaderValueType {
    type Error = ParseError;

    fn try_from(value: u8) -> ParseResult<Self> {
        match value {
            0 => Ok(Self::BoolTrue),
            1 => Ok(Self::BoolFalse),
            2 => Ok(Self::Byte),
            3 => Ok(Self::Short),
            4 => Ok(Self::Integer),
            5 => Ok(Self::Long),
            6 => Ok(Self::ByteArray),
            7 => Ok(Self::String),
            8 => Ok(Self::Timestamp),
            9 => Ok(Self::Uuid),
            _ => Err(ParseError::InvalidHeaderType(value)),
        }
    }
}

/// 头部值
///
/// 支持 AWS Event Stream 协议定义的所有值类型
#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    ByteArray(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl HeaderValue {
    /// 尝试获取字符串值
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }
}

/// 消息头部集合
#[derive(Debug, Clone, Default)]
pub struct Headers {
    inner: HashMap<String, HeaderValue>,
}

impl Headers {
    /// 创建空的头部集合
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// 插入头部
    pub fn insert(&mut self, name: String, value: HeaderValue) {
        self.inner.insert(name, value);
    }

    /// 获取头部值
    pub fn get(&self, name: &str) -> Option<&HeaderValue> {
        self.inner.get(name)
    }

    /// 获取字符串类型的头部值
    pub fn get_string(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| v.as_str())
    }

    /// 获取消息类型 (:message-type)
    pub fn message_type(&self) -> Option<&str> {
        self.get_string(":message-type")
    }

    /// 获取事件类型 (:event-type)
    pub fn event_type(&self) -> Option<&str> {
        self.get_string(":event-type")
    }

    /// 获取异常类型 (:exception-type)
    pub fn exception_type(&self) -> Option<&str> {
        self.get_string(":exception-type")
    }

    /// 获取错误代码 (:error-code)
    pub fn error_code(&self) -> Option<&str> {
        self.get_string(":error-code")
    }
}

/// 从字节流解析头部
///
/// # Arguments
/// * `data` - 头部数据切片
/// * `header_length` - 头部总长度
///
/// # Returns
/// 解析后的 Headers 结构
pub fn parse_headers(data: &[u8], header_length: usize) -> ParseResult<Headers> {
    // 验证数据长度是否足够
    if data.len() < header_length {
        return Err(ParseError::Incomplete {
            needed: header_length,
            available: data.len(),
        });
    }

    // 头部段之外的字节（payload）不得被读到：值是变长字段（长度前缀 + N 字节），只按
    // `data.len()` 设界时，一个被污染的「值长度」就能把 payload 读进头部并**照常返回 Ok**。
    // 调用点 frame.rs 传的已是切好的窗口（data.len() == header_length），但本函数是 pub
    // API，不能靠调用方自觉。
    let data = &data[..header_length];

    let mut headers = Headers::new();
    let mut offset = 0;

    while offset < header_length {
        // 读取头部名称长度 (1 byte)
        //
        // 切片后此处不可达（offset < header_length == data.len()）；保留为**错误**而非
        // `break`：截断的头部必须报错，绝不能静默当成「解析成功，字段就这些」。
        if offset >= data.len() {
            return Err(ParseError::Incomplete {
                needed: header_length,
                available: data.len(),
            });
        }
        let name_len = data[offset] as usize;
        offset += 1;

        // 验证名称长度
        if name_len == 0 {
            return Err(ParseError::HeaderParseFailed(
                "头部名称长度不能为 0".to_string(),
            ));
        }

        // 读取头部名称
        if offset + name_len > data.len() {
            return Err(ParseError::Incomplete {
                needed: name_len,
                available: data.len() - offset,
            });
        }
        let name = std::str::from_utf8(&data[offset..offset + name_len])
            .map_err(|_| {
                // 头部名必须是合法 UTF-8（协议规定）。`from_utf8_lossy` 会把非法字节换成
                // U+FFFD 并**照常成功**：不同的坏字节序列因此塌缩成同一个名字，可能顶替
                // `:message-type` / `:event-type` 这类路由头。畸形帧就该报错。
                ParseError::HeaderParseFailed("头部名称不是合法 UTF-8".to_string())
            })?
            .to_string();
        offset += name_len;

        // 读取值类型 (1 byte)
        if offset >= data.len() {
            return Err(ParseError::Incomplete {
                needed: 1,
                available: 0,
            });
        }
        let value_type = HeaderValueType::try_from(data[offset])?;
        offset += 1;

        // 根据类型解析值
        let value = parse_header_value(&data[offset..], value_type, &mut offset)?;
        // 头部名必须唯一（AWS Event Stream 规定）。重复名在 `HashMap::insert` 下是**后写
        // 覆盖先写**，而 `:message-type` / `:event-type` / `:exception-type` / `:error-code`
        // 正是决定本帧如何分类的路由头 —— 一条重复头就能改写分类结果（先 `event` 再
        // `error` 即可把错误帧伪装成正常事件）。重复即畸形帧：报错，交给解码器的
        // 「整帧跳过」恢复路径，而不是悄悄接受。
        if headers.get(&name).is_some() {
            return Err(ParseError::HeaderParseFailed(format!(
                "重复的头部名称: {name}"
            )));
        }
        headers.insert(name, value);
    }

    Ok(headers)
}

/// 解析头部值
fn parse_header_value(
    data: &[u8],
    value_type: HeaderValueType,
    global_offset: &mut usize,
) -> ParseResult<HeaderValue> {
    let mut local_offset = 0;

    let result = match value_type {
        HeaderValueType::BoolTrue => Ok(HeaderValue::Bool(true)),
        HeaderValueType::BoolFalse => Ok(HeaderValue::Bool(false)),
        HeaderValueType::Byte => {
            ensure_bytes(data, 1)?;
            let v = data[0] as i8;
            local_offset = 1;
            Ok(HeaderValue::Byte(v))
        }
        HeaderValueType::Short => {
            ensure_bytes(data, 2)?;
            let v = i16::from_be_bytes([data[0], data[1]]);
            local_offset = 2;
            Ok(HeaderValue::Short(v))
        }
        HeaderValueType::Integer => {
            ensure_bytes(data, 4)?;
            let v = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            local_offset = 4;
            Ok(HeaderValue::Integer(v))
        }
        HeaderValueType::Long => {
            ensure_bytes(data, 8)?;
            let v = i64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            local_offset = 8;
            Ok(HeaderValue::Long(v))
        }
        HeaderValueType::Timestamp => {
            ensure_bytes(data, 8)?;
            let v = i64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            local_offset = 8;
            Ok(HeaderValue::Timestamp(v))
        }
        HeaderValueType::ByteArray => {
            ensure_bytes(data, 2)?;
            let len = u16::from_be_bytes([data[0], data[1]]) as usize;
            ensure_bytes(data, 2 + len)?;
            let v = data[2..2 + len].to_vec();
            local_offset = 2 + len;
            Ok(HeaderValue::ByteArray(v))
        }
        HeaderValueType::String => {
            ensure_bytes(data, 2)?;
            let len = u16::from_be_bytes([data[0], data[1]]) as usize;
            ensure_bytes(data, 2 + len)?;
            // 同头部名：字符串值也必须是合法 UTF-8。非法字节不得被 U+FFFD 静默替换 ——
            // 那会让两个不同的字节串在下游比较时相等（`:error-code` / `:exception-type`
            // 的分类与日志因此失真）。
            let v = std::str::from_utf8(&data[2..2 + len])
                .map_err(|_| {
                    ParseError::HeaderParseFailed("头部字符串值不是合法 UTF-8".to_string())
                })?
                .to_string();
            local_offset = 2 + len;
            Ok(HeaderValue::String(v))
        }
        HeaderValueType::Uuid => {
            ensure_bytes(data, 16)?;
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&data[..16]);
            local_offset = 16;
            Ok(HeaderValue::Uuid(uuid))
        }
    };

    *global_offset += local_offset;
    result
}

/// 确保有足够的字节
fn ensure_bytes(data: &[u8], needed: usize) -> ParseResult<()> {
    if data.len() < needed {
        Err(ParseError::Incomplete {
            needed,
            available: data.len(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_value_type_conversion() {
        assert_eq!(
            HeaderValueType::try_from(0).unwrap(),
            HeaderValueType::BoolTrue
        );
        assert_eq!(
            HeaderValueType::try_from(7).unwrap(),
            HeaderValueType::String
        );
        assert!(HeaderValueType::try_from(10).is_err());
    }

    #[test]
    fn test_header_value_as_str() {
        let value = HeaderValue::String("test".to_string());
        assert_eq!(value.as_str(), Some("test"));

        let value = HeaderValue::Bool(true);
        assert_eq!(value.as_str(), None);
    }

    #[test]
    fn test_headers_get_string() {
        let mut headers = Headers::new();
        headers.insert(
            ":message-type".to_string(),
            HeaderValue::String("event".to_string()),
        );
        assert_eq!(headers.message_type(), Some("event"));
    }

    #[test]
    fn test_parse_headers_string() {
        // 构造一个简单的头部: name_len(1) + name + type(7=string) + value_len(2) + value
        // 头部名: "x" (长度 1)
        // 值类型: 7 (String)
        // 值: "ab" (长度 2)
        let data = [1u8, b'x', 7, 0, 2, b'a', b'b'];
        let headers = parse_headers(&data, data.len()).unwrap();
        assert_eq!(headers.get_string("x"), Some("ab"));
    }

    /// 头部名必须是合法 UTF-8：非法字节不得被 lossy 替换成 U+FFFD 后照常成功
    /// （不同的坏字节会塌缩成同一个名字，可能顶替 `:message-type` 这类路由头）。
    #[test]
    fn test_parse_headers_rejects_invalid_utf8_name() {
        // name_len(2) + 非法字节 0xFF 0xFE + type(7) + len(0)
        let data = [2u8, 0xFF, 0xFE, 7, 0, 0];
        assert!(matches!(
            parse_headers(&data, data.len()),
            Err(ParseError::HeaderParseFailed(_))
        ));
    }

    /// 字符串值同口径：非法 UTF-8 必须报错，而不是静默变成 U+FFFD。
    #[test]
    fn test_parse_headers_rejects_invalid_utf8_value() {
        // name("x") + type(7) + len(1) + 非法字节 0xFF
        let data = [1u8, b'x', 7, 0, 1, 0xFF];
        assert!(matches!(
            parse_headers(&data, data.len()),
            Err(ParseError::HeaderParseFailed(_))
        ));
    }

    /// 重复头部名必须报错：`HashMap::insert` 是后写覆盖先写，而路由头决定帧分类
    /// （先 `event` 再 `error` 就能把错误帧伪装成正常事件）。
    #[test]
    fn test_parse_headers_rejects_duplicate_names() {
        let data = [
            1u8, b'x', 7, 0, 1, b'a', // x = "a"
            1u8, b'x', 7, 0, 1, b'b', // x = "b"（重复）
        ];
        assert!(matches!(
            parse_headers(&data, data.len()),
            Err(ParseError::HeaderParseFailed(_))
        ));
    }

    /// 头部段之外的字节（payload）不得被读进头部。
    ///
    /// 值长度是变长字段：只按 `data.len()` 设界时，一个「恰好跨到 payload 里」的长度会被
    /// 整段读走并**照常返回 Ok**（本用例改前就会这样），等于让被污染的头部把 payload 吃掉。
    /// 回退即 FAIL：去掉 `&data[..header_length]` 切片后，下面第一个断言会变成 Ok。
    #[test]
    fn test_parse_headers_does_not_read_payload_bytes() {
        // 头部段只声明 4 字节：name_len(1)="x" + type(7=String) + value_len(2)=16。
        // 值长度谎报 16 字节，后面紧跟的正是 payload。
        let mut data = vec![1u8, b'x', 7, 0, 16];
        data.extend_from_slice(b"0123456789ABCDEF"); // 16 字节 payload
        let err = parse_headers(&data, 4).unwrap_err();
        assert!(
            matches!(err, ParseError::Incomplete { .. }),
            "越界读 payload 必须报 Incomplete: {err:?}"
        );

        // 反过来：窗口切对了，尾部 payload 再长也不影响解析。
        let mut ok = vec![1u8, b'x', 7, 0, 2, b'a', b'b'];
        ok.extend_from_slice(b"PAYLOAD");
        let headers = parse_headers(&ok, 7).unwrap();
        assert_eq!(headers.get_string("x"), Some("ab"));
    }
}
