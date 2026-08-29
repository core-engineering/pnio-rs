//! NDR array-header codec for PNIO RPC bodies: five `u32` counters on requests, a status
//! plus four `u32` counters on responses, both in the DCE-RPC header's DREP byte order.

use super::{Drep, RpcError};

/// NDR request body header: `args_max`, `args_len`, then a conformant/varying array header
/// (`max_count`, `offset`, `actual_count`) in front of the PNIO block payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NdrRequest {
    pub args_max: u32,
    pub args_len: u32,
    pub max_count: u32,
    pub offset: u32,
    pub actual_count: u32,
}

impl NdrRequest {
    pub const LEN: usize = 20;

    /// Parses the 20-byte header and returns it along with the trailing block payload
    /// (`actual_count` bytes). Rejects a buffer shorter than [`Self::LEN`], `args_len !=
    /// actual_count`, a nonzero `offset`, or an `actual_count` that overruns the buffer.
    pub fn parse(buf: &[u8], drep: Drep) -> Result<(NdrRequest, &[u8]), RpcError> {
        if buf.len() < Self::LEN {
            return Err(RpcError::TooShort {
                need: Self::LEN,
                have: buf.len(),
            });
        }
        let args_max = drep.u32(&buf[0..4]);
        let args_len = drep.u32(&buf[4..8]);
        let max_count = drep.u32(&buf[8..12]);
        let offset = drep.u32(&buf[12..16]);
        let actual_count = drep.u32(&buf[16..20]);

        if args_len != actual_count {
            return Err(RpcError::NdrMismatch("args_len != actual_count"));
        }
        if offset != 0 {
            return Err(RpcError::NdrMismatch("array offset must be 0"));
        }
        let available = buf.len() - Self::LEN;
        if actual_count as usize > available {
            return Err(RpcError::NdrMismatch("actual_count overruns buffer"));
        }

        let n = NdrRequest {
            args_max,
            args_len,
            max_count,
            offset,
            actual_count,
        };
        let blocks = &buf[Self::LEN..Self::LEN + actual_count as usize];
        Ok((n, blocks))
    }

    /// Builds a request header for `blocks_len` bytes of PNIO block payload. `max_count`
    /// echoes `args_max` (the array's declared capacity, per the golden captures), while
    /// `args_len`/`actual_count` carry the actual payload length; `offset` is zero.
    pub fn for_blocks(args_max: u32, blocks_len: u32) -> NdrRequest {
        NdrRequest {
            args_max,
            args_len: blocks_len,
            max_count: args_max,
            offset: 0,
            actual_count: blocks_len,
        }
    }

    pub fn write(&self, out: &mut Vec<u8>, drep: Drep) {
        drep.put_u32(out, self.args_max);
        drep.put_u32(out, self.args_len);
        drep.put_u32(out, self.max_count);
        drep.put_u32(out, self.offset);
        drep.put_u32(out, self.actual_count);
    }
}

/// NDR response body header: a `status` word, then the same array header
/// (`args_len`/`max_count`/`offset`/`actual_count`) as [`NdrRequest`], minus `args_max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NdrResponse {
    pub status: u32,
    pub args_len: u32,
    pub max_count: u32,
    pub offset: u32,
    pub actual_count: u32,
}

impl NdrResponse {
    pub const LEN: usize = 20;

    /// Parses the 20-byte header and returns it along with the trailing block payload
    /// (`actual_count` bytes). Rejects a buffer shorter than [`Self::LEN`], `args_len !=
    /// actual_count`, a nonzero `offset`, or an `actual_count` that overruns the buffer.
    pub fn parse(buf: &[u8], drep: Drep) -> Result<(NdrResponse, &[u8]), RpcError> {
        if buf.len() < Self::LEN {
            return Err(RpcError::TooShort {
                need: Self::LEN,
                have: buf.len(),
            });
        }
        let status = drep.u32(&buf[0..4]);
        let args_len = drep.u32(&buf[4..8]);
        let max_count = drep.u32(&buf[8..12]);
        let offset = drep.u32(&buf[12..16]);
        let actual_count = drep.u32(&buf[16..20]);

        if args_len != actual_count {
            return Err(RpcError::NdrMismatch("args_len != actual_count"));
        }
        if offset != 0 {
            return Err(RpcError::NdrMismatch("array offset must be 0"));
        }
        let available = buf.len() - Self::LEN;
        if actual_count as usize > available {
            return Err(RpcError::NdrMismatch("actual_count overruns buffer"));
        }

        let n = NdrResponse {
            status,
            args_len,
            max_count,
            offset,
            actual_count,
        };
        let blocks = &buf[Self::LEN..Self::LEN + actual_count as usize];
        Ok((n, blocks))
    }

    /// Builds a success response header for `blocks_len` bytes of PNIO block payload.
    /// `max_count` echoes the request's `args_max` (p-net does this, and the golden
    /// captures require it), not `blocks_len`.
    pub fn ok(request_args_max: u32, blocks_len: u32) -> NdrResponse {
        NdrResponse {
            status: 0,
            args_len: blocks_len,
            max_count: request_args_max,
            offset: 0,
            actual_count: blocks_len,
        }
    }

    /// Builds an error response header: `status` nonzero, all lengths/counts zero except
    /// `max_count`, which still echoes the request's `args_max`.
    pub fn error(status: u32, request_args_max: u32) -> NdrResponse {
        NdrResponse {
            status,
            args_len: 0,
            max_count: request_args_max,
            offset: 0,
            actual_count: 0,
        }
    }

    pub fn write(&self, out: &mut Vec<u8>, drep: Drep) {
        drep.put_u32(out, self.status);
        drep.put_u32(out, self.args_len);
        drep.put_u32(out, self.max_count);
        drep.put_u32(out, self.offset);
        drep.put_u32(out, self.actual_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Drep, RpcHeader};
    use crate::testutil::{golden, RPC_OFF};

    const BODY: usize = RPC_OFF + RpcHeader::LEN;

    #[test]
    fn parse_connect_request_body_le() {
        let f = golden("connect_req");
        let (n, blocks) = NdrRequest::parse(&f[BODY..], Drep::LITTLE).unwrap();
        assert_eq!(
            (
                n.args_max,
                n.args_len,
                n.max_count,
                n.offset,
                n.actual_count
            ),
            (557, 557, 557, 0, 557)
        );
        assert_eq!(blocks.len(), 557);
        assert_eq!(&blocks[..4], &[0x01, 0x01, 0x00, 0x5b]); // ARBlockReq header
    }

    #[test]
    fn parse_connect_response_body_be() {
        let f = golden("connect_res");
        let (n, blocks) = NdrResponse::parse(&f[BODY..], Drep::BIG).unwrap();
        assert_eq!(n.status, 0);
        assert_eq!((n.args_len, n.max_count, n.actual_count), (90, 557, 90));
        assert_eq!(blocks.len(), 90);
    }

    #[test]
    fn response_ok_matches_golden_bytes() {
        let f = golden("connect_res");
        let mut out = Vec::new();
        NdrResponse::ok(557, 90).write(&mut out, Drep::BIG);
        assert_eq!(out, &f[BODY..BODY + 20]);
    }

    #[test]
    fn request_for_blocks_matches_appready_golden() {
        let f = golden("appready_req");
        let mut out = Vec::new();
        NdrRequest::for_blocks(1340, 32).write(&mut out, Drep::BIG);
        assert_eq!(out, &f[BODY..BODY + 20]);
    }

    #[test]
    fn mismatch_is_rejected() {
        let mut f = golden("connect_req")[BODY..].to_vec();
        f[16] = 0xff; // actual_count low byte (LE) -> 0x22ff > buffer
        assert!(matches!(
            NdrRequest::parse(&f, Drep::LITTLE),
            Err(RpcError::NdrMismatch(_))
        ));
        assert!(matches!(
            NdrRequest::parse(&f[..10], Drep::LITTLE),
            Err(RpcError::TooShort { .. })
        ));
    }
}
