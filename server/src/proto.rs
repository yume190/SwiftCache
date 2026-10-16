//! Generated gRPC bindings for the LLVM remote caching service protocol.
//!
//! The `.proto` files are verbatim copies of
//! `llvm/lib/RemoteCachingService/RemoteCacheProto/*.proto` from
//! swiftlang/llvm-project.

pub mod cas {
    tonic::include_proto!("compilation_cache_service.cas.v1");
}

pub mod kv {
    tonic::include_proto!("compilation_cache_service.keyvalue.v1");
}
