mod rsa;
mod xor;

use base64::{engine::general_purpose, Engine as _};
use rand::Rng;

use xor::*;

// 16 bytes
type Key = [u8; 16];

pub fn gen_key() -> Key {
    rand::thread_rng().gen::<Key>()
}

pub fn encode(input: &[u8], key: &[u8; 16]) -> String {
    let mut buf = Vec::with_capacity(16 + input.len());
    buf.extend_from_slice(key);
    buf.extend_from_slice(input);
    let payload = &mut buf[16..];
    xor_transform(payload, &xor_derive_key(key, 4));
    payload.reverse();
    xor_transform(payload, &XOR_CLIENT_KEY);
    let my_rsa = rsa::MyRSA::new();
    general_purpose::STANDARD.encode(my_rsa.encrypt(&buf))
}

pub fn decode(input: &str, key: &[u8; 16]) -> Result<Vec<u8>, base64::DecodeError> {
    let my_rsa = rsa::MyRSA::new();
    let buf = general_purpose::STANDARD.decode(input)?;
    let buf = my_rsa.decrypt(&buf);
    let mut output = buf[16..].to_vec();
    xor_transform(&mut output, &xor_derive_key(&buf[..16], 12));
    output.reverse();
    xor_transform(&mut output, &xor_derive_key(key, 4));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_matches_python_reference() {
        let key = *b"0123456789abcdef";
        let input = br#"{"ac":"add_task_urls","app_ver":"27.0.5.7","uid":"1","wp_path_id":"123","url[0]":"magnet:?xt=urn:btih:hash-a"}"#;
        let mut payload = Vec::with_capacity(16 + input.len());
        payload.extend_from_slice(&key);
        payload.extend_from_slice(input);
        let body = &mut payload[16..];
        xor_transform(body, &xor_derive_key(&key, 4));
        body.reverse();
        xor_transform(body, &XOR_CLIENT_KEY);

        let encoded = general_purpose::STANDARD
            .encode(rsa::MyRSA::new().encrypt_with_deterministic_padding(&payload));
        assert_eq!(
            encoded,
            "HbVwWscVnKS/h6mPfnahPIEUKEqZ8g1ddlxPNj+S+ZQZaq5TDQb2XB0KFql7YguwUWYfDFFkh+NSXpzBuKIxuDs0FMAuSabUkzmBkbpP1oL5nh1W3s5G2HnG801jsFc4hS7huW237KDLjv4isv8bqTQqeqp5c2ZylZ5I2QbJ//6F3Znj/CQvTxINq1rFWXnY8scV/vxNe1cdXtu2CEz+u+Beddh4Eg4BUwsOhr1hMCf8i2dM6Z+0vD/Dam7uEnhmujzLKp/Vr/ANJ+k20nry9edNdB+CJN1YQC+jmQ/6oN57I0wZx8vt57Tige1QXjdMDzVdRMcOOVw+Fq/GFfUsgQ=="
        );
    }
}
