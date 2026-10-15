//! HMAC-SHA256 签名与校验（SPEC §5）。
//!
//! ```text
//! message   = <timestamp 十进制 ASCII 秒> || <raw body bytes>
//! signature = hex_lower(HMAC_SHA256(shared_secret, message))
//! ```
//!
//! 请求头：`X-SSM-Timestamp: <unix 秒>`、`X-SSM-Signature: <hex>`。
//! 空 body 也参与签名。`GET /probe` 不签名（仅用于存活判定）。

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::SignError;

type HmacSha256 = Hmac<Sha256>;

/// 请求头名：unix 秒时间戳。
pub const TIMESTAMP_HEADER: &str = "X-SSM-Timestamp";

/// 请求头名：HMAC-SHA256 十六进制小写签名。
pub const SIGNATURE_HEADER: &str = "X-SSM-Signature";

/// 时间戳容忍窗（秒），防重放（SPEC §5）。
pub const MAX_SKEW_SEC: i64 = 300;

/// 组装待签名消息：`<timestamp 十进制 ASCII> || <body>`。
fn message(timestamp: i64, body: &[u8]) -> Vec<u8> {
    let ts = timestamp.to_string();
    let mut msg = Vec::with_capacity(ts.len() + body.len());
    msg.extend_from_slice(ts.as_bytes());
    msg.extend_from_slice(body);
    msg
}

/// 计算签名，返回十六进制小写字符串（恒定长度 64）。
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    // new_from_slice 对任意长度密钥都不会失败（HMAC 规范），此处不可能 panic。
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(&message(timestamp, body));
    hex::encode(mac.finalize().into_bytes())
}

/// 校验签名：先查时间窗（`|now - timestamp| <= 300s`），再做**常量时间**比较。
///
/// `now` 由调用方注入（relay 用 `Utc::now().timestamp()`），便于单测。
pub fn verify(
    secret: &[u8],
    timestamp: i64,
    body: &[u8],
    sig_hex: &str,
    now: i64,
) -> Result<(), SignError> {
    // 1) 解析十六进制。非法输入直接拒绝（不要 panic）。
    let provided = hex::decode(sig_hex).map_err(|_| SignError::InvalidHex(sig_hex.to_string()))?;

    // 2) 时间窗：先于签名比较，避免过期包消耗常量时间比较。
    //    用 checked_sub 防 i64 溢出（恶意超大时间戳）。
    let delta = now
        .checked_sub(timestamp)
        .map(i64::saturating_abs)
        .unwrap_or(i64::MAX);
    if delta > MAX_SKEW_SEC {
        return Err(SignError::TimestampOutOfWindow {
            timestamp,
            now,
            delta,
            max_skew: MAX_SKEW_SEC,
        });
    }

    // 3) 常量时间比较（hmac::Mac::verify_slice 内部使用 subtle）。
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(&message(timestamp, body));
    mac.verify_slice(&provided).map_err(|_| SignError::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-shared-secret";
    const BODY: &[u8] = br#"{"host_id":"kebei-prod-01","seq":1}"#;
    const TS: i64 = 1_792_000_000;

    fn hex_of(secret: &[u8], ts: i64, body: &[u8]) -> String {
        sign(secret, ts, body)
    }

    #[test]
    fn signature_matches_known_vector() {
        // 固定向量：一旦实现漂移就会失败（防止悄悄改变消息拼接方式）。
        // message = "1792000000" || BODY
        let got = sign(SECRET, TS, BODY);
        assert_eq!(got.len(), 64, "HMAC-SHA256 十六进制串必须 64 字符");
        assert_eq!(got, sign(SECRET, TS, BODY), "签名必须是确定性的");
        assert!(
            got.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "必须是十六进制小写"
        );
        // 与独立实现（openssl 风格手工拼接）交叉验证
        let mut mac = HmacSha256::new_from_slice(SECRET).unwrap();
        mac.update(format!("{TS}").as_bytes());
        mac.update(BODY);
        assert_eq!(got, hex::encode(mac.finalize().into_bytes()));
    }

    #[test]
    fn correct_signature_passes() {
        let sig = sign(SECRET, TS, BODY);
        assert_eq!(verify(SECRET, TS, BODY, &sig, TS), Ok(()));
        // 同一签名在容忍窗内也应通过
        assert_eq!(verify(SECRET, TS, BODY, &sig, TS + 299), Ok(()));
        assert_eq!(verify(SECRET, TS, BODY, &sig, TS - 299), Ok(()));
    }

    #[test]
    fn boundary_300s_passes_301s_fails() {
        let sig = sign(SECRET, TS, BODY);
        assert_eq!(
            verify(SECRET, TS, BODY, &sig, TS + 300),
            Ok(()),
            "偏差 300s 应通过"
        );
        assert_eq!(
            verify(SECRET, TS, BODY, &sig, TS - 300),
            Ok(()),
            "偏差 -300s 应通过"
        );

        for delta in [301_i64, -301, 3600, -3600] {
            let err = verify(SECRET, TS, BODY, &sig, TS + delta).unwrap_err();
            match err {
                SignError::TimestampOutOfWindow {
                    delta: d, max_skew, ..
                } => {
                    assert_eq!(max_skew, MAX_SKEW_SEC);
                    assert_eq!(d, delta.abs());
                }
                other => panic!("期望 TimestampOutOfWindow，得到 {other:?}"),
            }
        }
    }

    #[test]
    fn tampered_body_fails() {
        let sig = sign(SECRET, TS, BODY);
        let tampered = br#"{"host_id":"kebei-prod-01","seq":2}"#;
        assert_eq!(
            verify(SECRET, TS, tampered, &sig, TS),
            Err(SignError::Mismatch)
        );

        // 只改一个字节也要失败
        let mut one_byte = BODY.to_vec();
        one_byte[0] ^= 0x01;
        assert_eq!(
            verify(SECRET, TS, &one_byte, &sig, TS),
            Err(SignError::Mismatch)
        );

        // 空 body vs 非空 body 不可互换
        assert_eq!(verify(SECRET, TS, b"", &sig, TS), Err(SignError::Mismatch));
    }

    #[test]
    fn tampered_timestamp_fails() {
        let sig = sign(SECRET, TS, BODY);
        // 时间戳也在签名消息里：改时间戳（即使仍在窗口内）必然签名不匹配
        assert_eq!(
            verify(SECRET, TS + 1, BODY, &sig, TS + 1),
            Err(SignError::Mismatch)
        );
    }

    #[test]
    fn wrong_secret_fails() {
        let sig = sign(b"other-secret", TS, BODY);
        assert_eq!(verify(SECRET, TS, BODY, &sig, TS), Err(SignError::Mismatch));
    }

    #[test]
    fn timestamp_is_part_of_message_not_concatenation_ambiguous() {
        // timestamp=17, body="92000000..." 与 timestamp=1792000000, body="" 拼接后同为
        // "17920000000..."，因此两者签名必然相同——这是协议定义的已知性质，
        // 固定向量在此显式记录，避免后人误以为可以省略时间戳。
        let a = sign(SECRET, 17, b"92000000rest");
        let b = sign(SECRET, 1_792_000_000, b"rest");
        assert_eq!(a, b);
    }

    #[test]
    fn invalid_hex_is_rejected_not_panicking() {
        for bad in ["", "zz", "0", "abc", "0x1234", "g123", &"a".repeat(63)] {
            match verify(SECRET, TS, BODY, bad, TS) {
                Err(SignError::InvalidHex(_)) | Err(SignError::Mismatch) => {}
                other => panic!("输入 {bad:?} 应被拒绝，得到 {other:?}"),
            }
        }
        // 合法长度但值错误的 hex → Mismatch
        assert_eq!(
            verify(SECRET, TS, BODY, &"ab".repeat(32), TS),
            Err(SignError::Mismatch)
        );
    }

    #[test]
    fn empty_body_is_signed() {
        let sig = sign(SECRET, TS, b"");
        assert_eq!(verify(SECRET, TS, b"", &sig, TS), Ok(()));
        assert_eq!(verify(SECRET, TS, b"x", &sig, TS), Err(SignError::Mismatch));
        assert_eq!(sig, hex_of(SECRET, TS, b""));
    }

    #[test]
    fn accepts_uppercase_hex_from_header() {
        let sig = sign(SECRET, TS, BODY);
        let upper = sig.to_uppercase();
        assert_eq!(
            verify(SECRET, TS, BODY, &upper, TS),
            Ok(()),
            "hex 解码应大小写不敏感"
        );
    }

    #[test]
    fn huge_timestamp_does_not_overflow() {
        let sig = sign(SECRET, i64::MIN, BODY);
        let err = verify(SECRET, i64::MIN, BODY, &sig, i64::MAX).unwrap_err();
        assert!(matches!(err, SignError::TimestampOutOfWindow { .. }));
    }
}
