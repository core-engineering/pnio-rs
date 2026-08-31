//! IODControlReq / IODControlRes: PrmEnd, ApplicationReady, and Release control
//! blocks (IEC 61158-6-10 §4.10.3). All six variants (request/response for each
//! of the three commands) share one 26-byte body: `reserved u16, ar_uuid, session_key
//! u16, reserved u16, command u16, properties u16`.

use super::{ty, BlockError, BlockHeader, CmError, Cursor};
use crate::rpc::{Drep, Uuid};

/// `IODControlReq`/`Res` `ControlCommand` bit values.
pub mod cmd {
    /// `PrmEnd`: end of parameterization (request).
    pub const PRM_END: u16 = 0x0001;
    /// `ApplicationReady`: the device is ready for cyclic data (request, device-initiated).
    pub const APPLICATION_READY: u16 = 0x0002;
    /// `Release`: release the AR (request).
    pub const RELEASE: u16 = 0x0004;
    /// `Done`: the answering side's response to any of the above.
    pub const DONE: u16 = 0x0008;
}

/// Body length of every control block: 26 bytes after the 6-byte header
/// (`block_length` field = 28, which counts the 2 version bytes too).
const BODY_LEN: usize = 26;

/// One `IODControlReq`/`IODControlRes` block — PrmEnd, ApplicationReady, or Release,
/// request or response. All six variants share this one body shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlBlock {
    /// `BlockType`, identifying which of the six PrmEnd/ApplicationReady/Release
    /// request/response variants this is (see [`ty`]).
    pub block_type: u16,
    /// The AR this control block applies to.
    pub ar_uuid: Uuid,
    /// The AR's session key, echoed back on every control exchange.
    pub session_key: u16,
    /// `ControlCommand` bitmask; see [`cmd`].
    pub command: u16,
    /// `ControlBlockProperties`; always `0` on the exchanges this crate builds.
    pub properties: u16,
}

impl ControlBlock {
    /// `blocks` = the block header followed by its body (no RPC header/NDR).
    pub fn parse(blocks: &[u8]) -> Result<ControlBlock, CmError> {
        let (header, body) = BlockHeader::parse(blocks)?;
        if !is_control_type(header.block_type) {
            return Err(CmError::Block(BlockError::UnexpectedType {
                expected: ty::IOD_CONTROL_REQ_PRM_END,
                got: header.block_type,
            }));
        }
        if body.len() != BODY_LEN {
            return Err(CmError::Block(BlockError::Malformed(
                "control block body length",
            )));
        }
        let mut c = Cursor::new(body);
        let _reserved_1 = c.u16()?;
        let ar_uuid = c.uuid()?;
        let session_key = c.u16()?;
        let _reserved_2 = c.u16()?;
        let command = c.u16()?;
        let properties = c.u16()?;
        Ok(ControlBlock {
            block_type: header.block_type,
            ar_uuid,
            session_key,
            command,
            properties,
        })
    }

    /// Write the 6-byte block header followed by the 26-byte body.
    pub fn write(&self, out: &mut Vec<u8>) {
        BlockHeader::write(out, self.block_type, BODY_LEN as u16);
        out.extend_from_slice(&0u16.to_be_bytes()); // reserved
        self.ar_uuid.write(out, Drep::BIG);
        out.extend_from_slice(&self.session_key.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // reserved
        out.extend_from_slice(&self.command.to_be_bytes());
        out.extend_from_slice(&self.properties.to_be_bytes());
    }
}

fn is_control_type(t: u16) -> bool {
    matches!(
        t,
        ty::IOD_CONTROL_REQ_PRM_END
            | ty::IOD_CONTROL_RES_PRM_END
            | ty::IOX_BLOCK_REQ_APP_READY
            | ty::IOX_BLOCK_RES_APP_READY
            | ty::RELEASE_BLOCK_REQ
            | ty::RELEASE_BLOCK_RES
    )
}

/// Build the PrmEnd Done response (`0x8110`) answering a PrmEnd request: same
/// `ar_uuid`/`session_key`, `command = DONE`, `properties = 0`.
pub fn prm_end_done(req: &ControlBlock) -> ControlBlock {
    ControlBlock {
        block_type: ty::IOD_CONTROL_RES_PRM_END,
        ar_uuid: req.ar_uuid,
        session_key: req.session_key,
        command: cmd::DONE,
        properties: 0,
    }
}

/// Build the ApplicationReady request (`0x0112`) the device sends to the controller
/// once parameterization is done.
pub fn app_ready_req(ar_uuid: Uuid, session_key: u16) -> ControlBlock {
    ControlBlock {
        block_type: ty::IOX_BLOCK_REQ_APP_READY,
        ar_uuid,
        session_key,
        command: cmd::APPLICATION_READY,
        properties: 0,
    }
}

/// Build the Release Done response (`0x8114`) answering a Release request: same
/// `ar_uuid`/`session_key`, `command = DONE`, `properties = 0`.
pub fn release_done(req: &ControlBlock) -> ControlBlock {
    ControlBlock {
        block_type: ty::RELEASE_BLOCK_RES,
        ar_uuid: req.ar_uuid,
        session_key: req.session_key,
        command: cmd::DONE,
        properties: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cm::block::ty;
    use crate::testutil::golden;

    const BLOCKS: usize = 142;

    #[test]
    fn parse_prm_end_and_answer_byte_exact() {
        let req = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        assert_eq!(req.block_type, ty::IOD_CONTROL_REQ_PRM_END);
        assert_eq!(
            (req.session_key, req.command, req.properties),
            (2, cmd::PRM_END, 0)
        );
        let mut out = Vec::new();
        prm_end_done(&req).write(&mut out);
        assert_eq!(out, &golden("prmend_res")[BLOCKS..]);
        assert_eq!(out.len(), 32);
    }

    #[test]
    fn app_ready_request_byte_exact() {
        let req = ControlBlock::parse(&golden("prmend_req")[BLOCKS..]).unwrap();
        let mut out = Vec::new();
        app_ready_req(req.ar_uuid, req.session_key).write(&mut out);
        assert_eq!(out, &golden("appready_req")[BLOCKS..]);
    }

    #[test]
    fn parse_app_ready_response_from_cpu() {
        let res = ControlBlock::parse(&golden("appready_res")[BLOCKS..]).unwrap();
        assert_eq!(res.block_type, ty::IOX_BLOCK_RES_APP_READY);
        assert_eq!(res.command, cmd::DONE);
        assert_eq!(
            res.ar_uuid.to_string(),
            "e5e1aecc-b133-4b4d-b187-cc68b0211ed2"
        );
    }

    #[test]
    fn rejects_non_control_block() {
        assert!(matches!(
            ControlBlock::parse(&golden("write_req")[BLOCKS..]),
            Err(CmError::Block(BlockError::UnexpectedType { .. }))
        ));
    }
}
