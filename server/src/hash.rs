//! CAS object identity, byte-compatible with LLVM's builtin CAS.
//!
//! Mirrors `llvm::cas::BuiltinObjectHasher<BLAKE3>::hashObject`
//! (`llvm/include/llvm/CAS/BuiltinObjectHasher.h`):
//!
//! ```text
//! BLAKE3( le64(refs.len())
//!       || refs[0] || refs[1] || ...      // raw digest bytes, no length prefix
//!       || le64(data.len())
//!       || data )
//! ```
//!
//! Keeping this identical to LLVM means the CAS ids we hand back from
//! `Put`/`Save` are the same ids the compiler's local on-disk CAS computes, so
//! the two id spaces stay interchangeable.

/// Size of a builtin CAS digest (`llvm::cas::builtin::HashType`).
pub const DIGEST_SIZE: usize = 32;

pub fn hash_object(refs: &[Vec<u8>], data: &[u8]) -> [u8; DIGEST_SIZE] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(refs.len() as u64).to_le_bytes());
    for r in refs {
        hasher.update(r);
    }
    hasher.update(&(data.len() as u64).to_le_bytes());
    hasher.update(data);
    *hasher.finalize().as_bytes()
}

/// Abbreviated id for per-request logging: full ids are 64–130 hex chars and
/// drown out the rest of the line.
pub fn short_id(digest: &[u8]) -> String {
    let n = digest.len().min(6);
    format!("{}…", hex::encode(&digest[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hash of the empty object (no refs, no data). This is the value the
    /// client probes for when it determines the service's digest size, and it
    /// is the one vector that is cheap to cross-check against
    /// `llvm-cas --cas <dir> --make-blob </dev/null`.
    #[test]
    fn empty_object_matches_llvm() {
        let d = hash_object(&[], b"");
        assert_eq!(d.len(), DIGEST_SIZE);
        // BLAKE3(le64(0) || le64(0)) == BLAKE3(16 zero bytes)
        assert_eq!(d, *blake3::hash(&[0u8; 16]).as_bytes());
    }

    #[test]
    fn refs_are_not_length_prefixed() {
        let a = hash_object(&[vec![1u8; 32], vec![2u8; 32]], b"x");
        let mut flat = Vec::new();
        flat.extend_from_slice(&2u64.to_le_bytes());
        flat.extend_from_slice(&[1u8; 32]);
        flat.extend_from_slice(&[2u8; 32]);
        flat.extend_from_slice(&1u64.to_le_bytes());
        flat.extend_from_slice(b"x");
        assert_eq!(a, *blake3::hash(&flat).as_bytes());
    }
}
