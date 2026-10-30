//! Messages the bench sends and parses beyond the crawler's handshake: `ping`/`pong`, `getheaders`/
//! `headers` and `getmnlistd`/`mnlistdiff`. Everything parsed here comes from untrusted peers: counts
//! are checked against the bytes actually present before anything is allocated or looped over.
//!
//! Formats follow Dash Core (src/evo/smldiff.h, src/evo/simplifiedmns.h, src/evo/netinfo.h,
//! src/primitives/transaction.h, src/merkleblock.h).

use anyhow::{Result, bail};

use crate::p2p::{Reader, write_compact_size};

/// A block header on the wire: 80 bytes, hashed with X11.
pub type RawHeader = [u8; 80];
/// Hashes in internal (wire) byte order; RPC and the reference document print them reversed.
pub type Hash = [u8; 32];

/// Dash Core's MAX_HEADERS_RESULTS: a `headers` reply carries at most this many.
pub const MAX_HEADERS: usize = 2000;

/// Dash block hash: X11 of the 80-byte header. `rs-x11-hash` (the C sph implementation that
/// rust-dashcore's dashcore_hashes also uses) always reads exactly 80 bytes from its input pointer,
/// so the fixed-size argument is what keeps this sound.
pub fn block_hash(header: &RawHeader) -> Hash {
    rs_x11_hash::get_x11_hash(header)
}

pub fn prev_hash(header: &RawHeader) -> Hash {
    header[4..36].try_into().unwrap()
}

/// Internal-order hash from display (RPC) hex.
pub fn parse_hash_hex(s: &str) -> Option<Hash> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut h = [0u8; 32];
    for (i, b) in h.iter_mut().rev().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(h)
}

/// The nonce of a `ping` or `pong` (BIP31: both are exactly one u64).
pub fn parse_nonce(payload: &[u8]) -> Result<u64> {
    let Ok(b) = <[u8; 8]>::try_from(payload) else { bail!("ping/pong of {} bytes", payload.len()) };
    Ok(u64::from_le_bytes(b))
}

/// `getheaders` with a one-hash locator and no stop hash: "everything after `from`, up to 2000".
pub fn getheaders_payload(protocol: i32, from: &Hash) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + 1 + 64);
    p.extend_from_slice(&protocol.to_le_bytes());
    write_compact_size(&mut p, 1);
    p.extend_from_slice(from);
    p.extend_from_slice(&[0u8; 32]);
    p
}

/// `headers`: CompactSize count, then per header 80 bytes and a transaction count that must be 0.
pub fn parse_headers(payload: &[u8]) -> Result<Vec<RawHeader>> {
    let mut r = Reader::new(payload);
    let n = r.count(81)?;
    if n > MAX_HEADERS {
        bail!("headers with {n} entries");
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.take(80)?.try_into().unwrap());
        if r.compact_size()? != 0 {
            bail!("header with transactions");
        }
    }
    if !r.is_empty() {
        bail!("trailing bytes after headers");
    }
    Ok(out)
}

/// `getmnlistd`: base block hash (zeros = from the empty list) and the block to build the list at.
pub fn getmnlistd_payload(base: &Hash, block: &Hash) -> Vec<u8> {
    [&base[..], &block[..]].concat()
}

// Protocol versions that change the mnlistdiff and SML entry encodings (Dash Core src/version.h).
// Before 70228 entries weren't versioned on the wire, but their encoding still depended on the
// version: too ambiguous to parse, and such nodes can't follow today's chain anyway.
pub const MIN_MNLISTDIFF_PROTOCOL: i32 = 70228;
const MNLISTDIFF_VERSION_ORDER: i32 = 70229;

// ProTx versions (src/evo/types.h) and masternode types (src/evo/dmn_types.h).
const PROTX_BASIC_BLS: u16 = 2;
const PROTX_EXT_ADDR: u16 = 3;
const MN_TYPE_EVO: u16 = 1;

/// What we check of a `mnlistdiff`: its header, the coinbase, and every list entry. The quorum
/// sections after the list aren't parsed (they don't say anything about serving the list).
#[derive(Debug, PartialEq)]
pub struct MnListDiff {
    pub block_hash: Hash,
    pub deleted: usize,
    pub entries: usize,
}

/// Parses a `mnlistdiff` as serialized for a peer at `protocol` (the negotiated version).
pub fn parse_mnlistdiff(payload: &[u8], protocol: i32) -> Result<MnListDiff> {
    if protocol < MIN_MNLISTDIFF_PROTOCOL {
        bail!("unsupported protocol {protocol}");
    }
    let mut r = Reader::new(payload);
    if protocol >= MNLISTDIFF_VERSION_ORDER {
        r.u16()?; // nVersion
    }
    r.hash()?; // baseBlockHash
    let block_hash = r.hash()?;
    skip_partial_merkle_tree(&mut r)?;
    skip_transaction(&mut r)?;
    if protocol < MNLISTDIFF_VERSION_ORDER {
        r.u16()?; // nVersion
    }
    let deleted = r.count(32)?;
    r.take(deleted * 32)?;
    // An entry is at least 2 + 32 + 32 + 1 + 48 + 20 + 1 bytes.
    let entries = r.count(136)?;
    for _ in 0..entries {
        skip_sml_entry(&mut r)?;
    }
    Ok(MnListDiff { block_hash, deleted, entries })
}

/// CPartialMerkleTree: transaction count, hashes, flag bits.
fn skip_partial_merkle_tree(r: &mut Reader) -> Result<()> {
    r.u32()?;
    let hashes = r.count(32)?;
    r.take(hashes * 32)?;
    r.var_bytes()?;
    Ok(())
}

/// A Dash transaction: version and type in one i32, inputs, outputs, lock time, and for special
/// transactions (version 3, type != 0) the extra payload.
fn skip_transaction(r: &mut Reader) -> Result<()> {
    let n_version = r.u32()?;
    let (version, tx_type) = (n_version & 0xffff, n_version >> 16);
    let inputs = r.count(41)?;
    for _ in 0..inputs {
        r.take(36)?; // prevout
        r.var_bytes()?; // scriptSig
        r.u32()?; // nSequence
    }
    let outputs = r.count(9)?;
    for _ in 0..outputs {
        r.u64()?; // value
        r.var_bytes()?; // scriptPubKey
    }
    r.u32()?; // nLockTime
    if version >= 3 && tx_type != 0 {
        r.var_bytes()?; // vExtraPayload
    }
    Ok(())
}

/// One CSimplifiedMNListEntry (as sent to peers at 70228 or later).
fn skip_sml_entry(r: &mut Reader) -> Result<()> {
    let version = r.u16()?;
    r.hash()?; // proRegTxHash
    r.hash()?; // confirmedHash
    if version >= PROTX_EXT_ADDR {
        skip_ext_net_info(r)?;
    } else {
        r.take(18)?; // CService, v1 encoding: 16-byte address, big-endian port
    }
    r.take(48)?; // pubKeyOperator
    r.take(20)?; // keyIDVoting
    let is_valid = r.u8()?;
    if is_valid > 1 {
        bail!("isValid byte {is_valid}");
    }
    if version >= PROTX_BASIC_BLS && r.u16()? == MN_TYPE_EVO {
        if version < PROTX_EXT_ADDR {
            r.u16()?; // platformHTTPPort
        }
        r.take(20)?; // platformNodeID
    }
    Ok(())
}

/// ExtNetInfo (ProTx version 3): a version byte, then (for version 1) purposes in ascending order,
/// each with a list of entries: an address in BIP155 form plus port, or a domain plus port.
fn skip_ext_net_info(r: &mut Reader) -> Result<()> {
    let version = r.u8()?;
    if version != 1 {
        return Ok(()); // Dash Core stops reading unknown versions here too.
    }
    let purposes = r.count(2)?;
    let mut last = None;
    for _ in 0..purposes {
        let purpose = r.u8()?;
        if last.is_some_and(|l| purpose <= l) {
            bail!("ExtNetInfo purposes out of order");
        }
        last = Some(purpose);
        let entries = r.count(1)?;
        for _ in 0..entries {
            match r.u8()? {
                // Service: BIP155 network ID, address, port.
                0x01 => {
                    r.u8()?;
                    skip_host_port(r, 512, "address")?;
                }
                0x02 => skip_host_port(r, 253, "domain")?,
                // Invalid entries are just the type byte.
                _ => {}
            }
        }
    }
    Ok(())
}

/// A length-prefixed host of at most `max` bytes, then a big-endian port.
fn skip_host_port(r: &mut Reader, max: u64, what: &str) -> Result<()> {
    let len = r.compact_size()?;
    if len > max {
        bail!("ExtNetInfo {what} too long");
    }
    r.take(len as usize + 2)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_hex(h: &Hash) -> String {
        h.iter().rev().map(|b| format!("{b:02x}")).collect()
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Mainnet genesis header (Dash Core chainparams).
    const GENESIS: &str = "010000000000000000000000000000000000000000000000000000000000000000000000c762a6567f3cc0\
        92f0684bb62b7e00a84890b990f07cc71a6bb58d64b98e02e0022ddb52f0ff0f1ec23fb901";
    /// Mainnet block 1, whose prev is the genesis hash.
    const BLOCK1: &str = "02000000b67a40f3cd5804437a108f105533739c37e6229bc1adcab385140b59fd0f0000a71c1aade44b\
        f8425bec0deb611c20b16da3442818ef20489ca1e2512be43eef814cdb52f0ff0f1edbf70100";

    #[test]
    fn x11_block_hashes() {
        let genesis: RawHeader = hex(GENESIS).try_into().unwrap();
        let h = block_hash(&genesis);
        assert_eq!(hash_hex(&h), "00000ffd590b1485b3caadc19b22e6379c733355108f107a430458cdf3407ab6");
        let block1: RawHeader = hex(BLOCK1).try_into().unwrap();
        assert_eq!(prev_hash(&block1), h);
        assert_eq!(hash_hex(&block_hash(&block1)), "000007d91d1254d60e2dd1ae580383070a4ddffa4c64c2eeb4a2f9ecc0414343");
        assert_eq!(parse_hash_hex(&hash_hex(&h)), Some(h));
        assert_eq!(parse_hash_hex("00"), None);
    }

    #[test]
    fn ping_pong_nonces() {
        assert_eq!(parse_nonce(&0x0102_0304_0506_0708u64.to_le_bytes()).unwrap(), 0x0102_0304_0506_0708);
        assert!(parse_nonce(&[1, 2, 3]).is_err());
        assert!(parse_nonce(&[0; 9]).is_err());
    }

    #[test]
    fn getheaders_layout() {
        let from = [7u8; 32];
        let p = getheaders_payload(70242, &from);
        assert_eq!(p.len(), 4 + 1 + 32 + 32);
        assert_eq!(&p[..4], &70242i32.to_le_bytes());
        assert_eq!(p[4], 1);
        assert_eq!(&p[5..37], &from);
        assert_eq!(&p[37..], &[0u8; 32]);
    }

    fn headers_msg(n: usize, tx_count: u8) -> Vec<u8> {
        let mut p = Vec::new();
        write_compact_size(&mut p, n as u64);
        for i in 0..n {
            p.extend_from_slice(&[i as u8; 80]);
            p.push(tx_count);
        }
        p
    }

    #[test]
    fn headers_parsing() {
        let h = parse_headers(&headers_msg(3, 0)).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[2], [2u8; 80]);
        assert!(parse_headers(&headers_msg(0, 0)).unwrap().is_empty());
        // Transactions, truncation, trailing bytes, too many, a count larger than the payload.
        assert!(parse_headers(&headers_msg(1, 1)).is_err());
        let m = headers_msg(2, 0);
        assert!(parse_headers(&m[..m.len() - 1]).is_err());
        assert!(parse_headers(&[m.clone(), vec![0]].concat()).is_err());
        assert!(parse_headers(&headers_msg(2001, 0)).unwrap_err().to_string().contains("2001"));
        assert!(parse_headers(&[0xfe, 0xff, 0xff, 0xff, 0x00]).unwrap_err().to_string().contains("exceeds"));
    }

    struct Diff(Vec<u8>);

    impl Diff {
        fn new(version_first: bool) -> Diff {
            let mut p = Vec::new();
            if version_first {
                p.extend_from_slice(&1u16.to_le_bytes());
            }
            p.extend_from_slice(&[0; 32]); // base
            p.extend_from_slice(&[9; 32]); // block
            // Merkle tree: 1 tx, 1 hash, 1 flag byte.
            p.extend_from_slice(&1u32.to_le_bytes());
            p.push(1);
            p.extend_from_slice(&[3; 32]);
            p.extend_from_slice(&[1, 1]);
            // Coinbase: version 3, type 5, one input, one output, payload of 4 bytes.
            p.extend_from_slice(&(3u32 | (5 << 16)).to_le_bytes());
            p.push(1);
            p.extend_from_slice(&[0; 36]);
            p.extend_from_slice(&[2, 0xaa, 0xbb]);
            p.extend_from_slice(&u32::MAX.to_le_bytes());
            p.push(1);
            p.extend_from_slice(&5u64.to_le_bytes());
            p.extend_from_slice(&[1, 0x51]);
            p.extend_from_slice(&0u32.to_le_bytes());
            p.extend_from_slice(&[4, 1, 2, 3, 4]);
            Diff(p)
        }

        fn entries(mut self, deleted: u8, entries: &[Vec<u8>]) -> Vec<u8> {
            self.0.push(deleted);
            for _ in 0..deleted {
                self.0.extend_from_slice(&[5; 32]);
            }
            self.0.push(entries.len() as u8);
            entries.iter().for_each(|e| self.0.extend_from_slice(e));
            // Quorum sections: not parsed.
            self.0.extend_from_slice(&[0, 0, 0]);
            self.0
        }
    }

    fn entry(version: u16, evo: bool, valid: bool) -> Vec<u8> {
        let mut e = version.to_le_bytes().to_vec();
        e.extend_from_slice(&[1; 32]);
        e.extend_from_slice(&[2; 32]);
        if version >= 3 {
            // ExtNetInfo v1: one purpose (core P2P) with an IPv4 service and a domain.
            e.extend_from_slice(&[1, 1, 0, 2]);
            e.extend_from_slice(&[0x01, 1, 4, 1, 2, 3, 4, 0x27, 0x0f]);
            e.extend_from_slice(&[0x02, 7]);
            e.extend_from_slice(b"dash.io");
            e.extend_from_slice(&[0x27, 0x0f]);
        } else {
            e.extend_from_slice(&[0; 18]);
        }
        e.extend_from_slice(&[3; 48]);
        e.extend_from_slice(&[4; 20]);
        e.push(valid as u8);
        if version >= 2 {
            e.extend_from_slice(&(evo as u16).to_le_bytes());
            if evo {
                if version < 3 {
                    e.extend_from_slice(&443u16.to_le_bytes());
                }
                e.extend_from_slice(&[6; 20]);
            }
        }
        e
    }

    #[test]
    fn mnlistdiff_parsing() {
        let entries = [entry(1, false, true), entry(2, false, false), entry(2, true, true), entry(3, true, true)];
        let msg = Diff::new(true).entries(0, &entries);
        let d = parse_mnlistdiff(&msg, 70242).unwrap();
        assert_eq!(d, MnListDiff { block_hash: [9; 32], deleted: 0, entries: 4 });
        // Old peers put nVersion after the coinbase.
        let mut old = Diff::new(false);
        old.0.extend_from_slice(&1u16.to_le_bytes());
        let d = parse_mnlistdiff(&old.entries(1, &entries), 70228).unwrap();
        assert_eq!((d.deleted, d.entries), (1, 4));
        assert!(parse_mnlistdiff(&msg, 70227).unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn mnlistdiff_rejects_malformed() {
        let msg = Diff::new(true).entries(0, &[entry(3, true, true)]);
        // Every truncation inside the parsed part fails cleanly.
        let parsed_len = msg.len() - 3;
        for cut in 0..parsed_len {
            assert!(parse_mnlistdiff(&msg[..cut], 70242).is_err(), "cut at {cut}");
        }
        // A huge entry count is refused up front.
        let mut huge = Diff::new(true).0;
        huge.extend_from_slice(&[0, 0xfe, 0xff, 0xff, 0xff, 0x0f]);
        assert!(parse_mnlistdiff(&huge, 70242).unwrap_err().to_string().contains("exceeds"));
        // A bad isValid byte.
        let mut bad = entry(2, false, true);
        let at = bad.len() - 3;
        bad[at] = 7;
        assert!(parse_mnlistdiff(&Diff::new(true).entries(0, &[bad]), 70242).is_err());
    }
}
