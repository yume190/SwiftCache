// plugin_probe — exercise Xcode's libToolchainCASPlugin.dylib directly against
// a remote cache server, without going through a full compiler build.
//
// It is the fast feedback loop for server development: it drives exactly the
// llcas_* entry points the compiler uses, so whatever it proves about the
// server also holds for swiftc/clang.
//
//   plugin_probe write <plugin> <ondisk-dir> <socket>   # populate the remote
//   plugin_probe read  <plugin> <ondisk-dir> <socket>   # must be a fresh dir
//
// Build: cc -O2 -o plugin_probe tools/plugin_probe.c

#include <dlfcn.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct llcas_cas_options_s *llcas_cas_options_t;
typedef struct llcas_cas_s *llcas_cas_t;

typedef struct {
  const uint8_t *data;
  size_t size;
} llcas_digest_t;

typedef struct {
  const void *data;
  size_t size;
} llcas_data_t;

typedef struct {
  uint64_t opaque;
} llcas_objectid_t;

typedef struct {
  uint64_t opaque;
} llcas_loaded_object_t;

typedef struct {
  uint64_t opaque_b;
  uint64_t opaque_e;
} llcas_object_refs_t;

typedef enum {
  LLCAS_LOOKUP_RESULT_SUCCESS = 0,
  LLCAS_LOOKUP_RESULT_NOTFOUND = 1,
  LLCAS_LOOKUP_RESULT_ERROR = 2,
} llcas_lookup_result_t;

#define SYM(name, type)                                                        \
  static type name;                                                            \
  name = (type)dlsym(h, #name);                                                \
  if (!name) {                                                                 \
    fprintf(stderr, "missing symbol %s\n", #name);                             \
    return 2;                                                                  \
  }

typedef void (*fn_get_plugin_version)(unsigned *, unsigned *);
typedef void (*fn_string_dispose)(char *);
typedef llcas_cas_options_t (*fn_options_create)(void);
typedef void (*fn_options_set_client_version)(llcas_cas_options_t, unsigned,
                                              unsigned);
typedef void (*fn_options_set_ondisk_path)(llcas_cas_options_t, const char *);
typedef bool (*fn_options_set_option)(llcas_cas_options_t, const char *,
                                      const char *, char **);
typedef llcas_cas_t (*fn_cas_create)(llcas_cas_options_t, char **);
typedef char *(*fn_get_hash_schema_name)(llcas_cas_t);
typedef bool (*fn_store_object)(llcas_cas_t, llcas_data_t,
                                const llcas_objectid_t *, size_t,
                                llcas_objectid_t *, char **);
typedef llcas_lookup_result_t (*fn_load_object)(llcas_cas_t, llcas_objectid_t,
                                                llcas_loaded_object_t *,
                                                char **);
typedef llcas_digest_t (*fn_objectid_get_digest)(llcas_cas_t, llcas_objectid_t);
typedef bool (*fn_cas_get_objectid)(llcas_cas_t, llcas_digest_t,
                                    llcas_objectid_t *, char **);
typedef llcas_data_t (*fn_loaded_object_get_data)(llcas_cas_t,
                                                  llcas_loaded_object_t);
typedef llcas_object_refs_t (*fn_loaded_object_get_refs)(llcas_cas_t,
                                                         llcas_loaded_object_t);
typedef size_t (*fn_object_refs_get_count)(llcas_cas_t, llcas_object_refs_t);
typedef llcas_objectid_t (*fn_object_refs_get_id)(llcas_cas_t,
                                                  llcas_object_refs_t, size_t);
typedef llcas_lookup_result_t (*fn_ac_get)(llcas_cas_t, llcas_digest_t,
                                           llcas_objectid_t *, bool, char **);
typedef bool (*fn_ac_put)(llcas_cas_t, llcas_digest_t, llcas_objectid_t, bool,
                          char **);
typedef void (*fn_cas_dispose)(llcas_cas_t);

static void print_hex(const char *label, const uint8_t *p, size_t n) {
  printf("%s", label);
  for (size_t i = 0; i < n; ++i)
    printf("%02x", p[i]);
  printf("\n");
}

// A fixed, arbitrary action-cache key; stable across runs so `read` can find
// what `write` stored.
static const uint8_t kACKey[32] = {
    0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12,
    0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c};

static const char kLeafTag[] = "swiftcache leaf payload";
static const char kRootTag[] = "swiftcache root payload";

// Deterministic filler so `write` and `read` agree on the expected bytes.
static char *make_payload(const char *tag, size_t size) {
  size_t tag_len = strlen(tag);
  if (size < tag_len)
    size = tag_len;
  char *buf = malloc(size + 1);
  memcpy(buf, tag, tag_len);
  for (size_t i = tag_len; i < size; ++i)
    buf[i] = (char)('a' + ((i * 31 + 7) % 26));
  buf[size] = '\0';
  return buf;
}

int main(int argc, char **argv) {
  if (argc < 5 || argc > 6) {
    fprintf(stderr,
            "usage: %s <write|read> <plugin.dylib> <ondisk-dir> <socket> "
            "[payload-bytes]\n",
            argv[0]);
    return 1;
  }
  const char *mode = argv[1];
  const char *plugin = argv[2];
  const char *ondisk = argv[3];
  const char *socket = argv[4];
  size_t payload_size = argc == 6 ? (size_t)strtoull(argv[5], NULL, 10) : 0;
  char *leaf_payload = make_payload(kLeafTag, payload_size);
  // The plugin refuses to upload a large node that also has references
  // ("data part is large with non-zero references"), which mirrors how real
  // cache entries are shaped: a small root pointing at large leaves.
  char *root_payload = make_payload(kRootTag, 0);
  size_t leaf_len = strlen(leaf_payload);
  size_t root_len = strlen(root_payload);
  printf("payload: leaf %zu bytes, root %zu bytes\n", leaf_len, root_len);

  void *h = dlopen(plugin, RTLD_LAZY);
  if (!h) {
    fprintf(stderr, "dlopen failed: %s\n", dlerror());
    return 2;
  }

  SYM(llcas_get_plugin_version, fn_get_plugin_version);
  SYM(llcas_string_dispose, fn_string_dispose);
  SYM(llcas_cas_options_create, fn_options_create);
  SYM(llcas_cas_options_set_client_version, fn_options_set_client_version);
  SYM(llcas_cas_options_set_ondisk_path, fn_options_set_ondisk_path);
  SYM(llcas_cas_options_set_option, fn_options_set_option);
  SYM(llcas_cas_create, fn_cas_create);
  SYM(llcas_cas_get_hash_schema_name, fn_get_hash_schema_name);
  SYM(llcas_cas_store_object, fn_store_object);
  SYM(llcas_cas_load_object, fn_load_object);
  SYM(llcas_objectid_get_digest, fn_objectid_get_digest);
  SYM(llcas_cas_get_objectid, fn_cas_get_objectid);
  SYM(llcas_loaded_object_get_data, fn_loaded_object_get_data);
  SYM(llcas_loaded_object_get_refs, fn_loaded_object_get_refs);
  SYM(llcas_object_refs_get_count, fn_object_refs_get_count);
  SYM(llcas_object_refs_get_id, fn_object_refs_get_id);
  SYM(llcas_actioncache_get_for_digest, fn_ac_get);
  SYM(llcas_actioncache_put_for_digest, fn_ac_put);
  SYM(llcas_cas_dispose, fn_cas_dispose);

  unsigned major = 0, minor = 0;
  llcas_get_plugin_version(&major, &minor);
  printf("plugin version: %u.%u\n", major, minor);

  llcas_cas_options_t opts = llcas_cas_options_create();
  llcas_cas_options_set_client_version(opts, 0, 3);
  llcas_cas_options_set_ondisk_path(opts, ondisk);
  char *err = NULL;
  if (llcas_cas_options_set_option(opts, "remote-service-path", socket, &err)) {
    fprintf(stderr, "set_option failed: %s\n", err ? err : "?");
    return 3;
  }

  llcas_cas_t cas = llcas_cas_create(opts, &err);
  if (!cas) {
    fprintf(stderr, "cas_create failed: %s\n", err ? err : "?");
    return 3;
  }
  char *schema = llcas_cas_get_hash_schema_name(cas);
  printf("hash schema: %s\n", schema ? schema : "(null)");
  if (schema)
    llcas_string_dispose(schema);

  llcas_digest_t key = {kACKey, sizeof(kACKey)};

  if (strcmp(mode, "write") == 0) {
    llcas_objectid_t leaf;
    llcas_data_t leaf_data = {leaf_payload, leaf_len};
    if (llcas_cas_store_object(cas, leaf_data, NULL, 0, &leaf, &err)) {
      fprintf(stderr, "store leaf failed: %s\n", err ? err : "?");
      return 4;
    }
    llcas_digest_t leaf_digest = llcas_objectid_get_digest(cas, leaf);
    print_hex("leaf  id: ", leaf_digest.data, leaf_digest.size);

    llcas_objectid_t root;
    llcas_data_t root_data = {root_payload, root_len};
    if (llcas_cas_store_object(cas, root_data, &leaf, 1, &root, &err)) {
      fprintf(stderr, "store root failed: %s\n", err ? err : "?");
      return 4;
    }
    llcas_digest_t root_digest = llcas_objectid_get_digest(cas, root);
    print_hex("root  id: ", root_digest.data, root_digest.size);

    if (llcas_actioncache_put_for_digest(cas, key, root, /*globally=*/true,
                                         &err)) {
      fprintf(stderr, "actioncache put failed: %s\n", err ? err : "?");
      return 4;
    }
    printf("wrote action cache entry\n");
    llcas_cas_dispose(cas);
    return 0;
  }

  if (strcmp(mode, "read") == 0) {
    llcas_objectid_t root;
    llcas_lookup_result_t r =
        llcas_actioncache_get_for_digest(cas, key, &root, /*globally=*/true, &err);
    if (r == LLCAS_LOOKUP_RESULT_ERROR) {
      fprintf(stderr, "actioncache get failed: %s\n", err ? err : "?");
      return 4;
    }
    if (r == LLCAS_LOOKUP_RESULT_NOTFOUND) {
      fprintf(stderr, "MISS: action cache key not found\n");
      return 5;
    }
    llcas_digest_t root_digest = llcas_objectid_get_digest(cas, root);
    print_hex("root  id: ", root_digest.data, root_digest.size);

    llcas_loaded_object_t obj;
    r = llcas_cas_load_object(cas, root, &obj, &err);
    if (r != LLCAS_LOOKUP_RESULT_SUCCESS) {
      fprintf(stderr, "MISS: root object not loadable (%d) %s\n", r,
              err ? err : "");
      return 5;
    }
    llcas_data_t d = llcas_loaded_object_get_data(cas, obj);
    printf("root data: %zu bytes, prefix '%.23s'\n", d.size,
           (const char *)d.data);

    llcas_object_refs_t refs = llcas_loaded_object_get_refs(cas, obj);
    size_t n = llcas_object_refs_get_count(cas, refs);
    printf("root refs: %zu\n", n);
    if (n != 1) {
      fprintf(stderr, "FAIL: expected 1 reference\n");
      return 5;
    }
    llcas_objectid_t leaf = llcas_object_refs_get_id(cas, refs, 0);
    llcas_loaded_object_t leaf_obj;
    r = llcas_cas_load_object(cas, leaf, &leaf_obj, &err);
    if (r != LLCAS_LOOKUP_RESULT_SUCCESS) {
      fprintf(stderr, "MISS: leaf object not loadable (%d) %s\n", r,
              err ? err : "");
      return 5;
    }
    llcas_data_t ld = llcas_loaded_object_get_data(cas, leaf_obj);
    printf("leaf data: %zu bytes, prefix '%.23s'\n", ld.size,
           (const char *)ld.data);

    if (ld.size != leaf_len || memcmp(ld.data, leaf_payload, ld.size) != 0) {
      fprintf(stderr, "FAIL: leaf payload mismatch\n");
      return 5;
    }
    if (d.size != root_len || memcmp(d.data, root_payload, d.size) != 0) {
      fprintf(stderr, "FAIL: root payload mismatch\n");
      return 5;
    }
    printf("OK: round-tripped through the remote cache\n");
    llcas_cas_dispose(cas);
    return 0;
  }

  fprintf(stderr, "unknown mode %s\n", mode);
  return 1;
}
