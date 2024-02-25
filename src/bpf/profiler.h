#include "basic_types.h"

#define MAX_UNWIND_INFO_SHARDS 25

// Number of frames to walk per tail call iteration.
#define MAX_STACK_DEPTH_PER_PROGRAM 7
// Number of BPF tail calls that will be attempted.
#define MAX_TAIL_CALLS 19
// Maximum number of frames.
#define MAX_STACK_DEPTH 127
_Static_assert(MAX_TAIL_CALLS *MAX_STACK_DEPTH_PER_PROGRAM >= MAX_STACK_DEPTH,
               "enough iterations to traverse the whole stack");
// Number of unique stacks.
#define MAX_STACK_TRACES_ENTRIES 64000
// Number of items in the stack counts aggregation map.
#define MAX_STACK_COUNTS_ENTRIES 10240
// Maximum number of processes we are willing to track.
#define MAX_PROCESSES 5000
// Binary search iterations for dwarf based stack walking.
// 2^19 can bisect ~524_288 entries.
#define MAX_BINARY_SEARCH_DEPTH 19
// Size of the unwind table.
// 250k * sizeof(stack_unwind_row_t) = 2MB
#define MAX_UNWIND_TABLE_SIZE 250 * 1000
_Static_assert(1 << MAX_BINARY_SEARCH_DEPTH >= MAX_UNWIND_TABLE_SIZE,
               "unwind table is big enough");

// Unwind tables bigger than can't fit in the remaining space
// of the current shard are broken up into chunks up to `MAX_UNWIND_TABLE_SIZE`.
#define MAX_UNWIND_TABLE_CHUNKS 30
// Maximum memory mappings per process.
#define MAX_MAPPINGS_PER_PROCESS 300

// Values for dwarf expressions.
#define DWARF_EXPRESSION_UNKNOWN 0
#define DWARF_EXPRESSION_PLT1 1
#define DWARF_EXPRESSION_PLT2 2

// Values for the unwind table's CFA type.
#define CFA_TYPE_RBP 1
#define CFA_TYPE_RSP 2
#define CFA_TYPE_EXPRESSION 3
// Special values.
#define CFA_TYPE_END_OF_FDE_MARKER 4

// Values for the unwind table's frame pointer type.
#define RBP_TYPE_UNCHANGED 0
#define RBP_TYPE_OFFSET 1
#define RBP_TYPE_REGISTER 2
#define RBP_TYPE_EXPRESSION 3
// Special values.
#define RBP_TYPE_UNDEFINED_RETURN_ADDRESS 4

// Binary search error codes.
#define BINARY_SEARCH_DEFAULT 0xFABADAFABADAULL
#define BINARY_SEARCH_SHOULD_NEVER_HAPPEN 0xDEADBEEFDEADBEEFULL
#define BINARY_SEARCH_EXHAUSTED_ITERATIONS 0xBADFADBADFADBADULL

#define REQUEST_UNWIND_INFORMATION (1ULL << 63)
#define REQUEST_PROCESS_MAPPINGS (1ULL << 62)
#define REQUEST_REFRESH_PROCINFO (1ULL << 61)

#define ENABLE_STATS_PRINTING false

// Stack walking methods.
enum stack_walking_method {
  STACK_WALKING_METHOD_FP = 0,
  STACK_WALKING_METHOD_DWARF = 1,
};

struct unwinder_config_t {
  bool filter_processes;
  bool verbose_logging;
};

struct unwinder_stats_t {
  u64 total;
  u64 success_dwarf;
  u64 error_truncated;
  u64 error_unsupported_expression;
  u64 error_unsupported_frame_pointer_action;
  u64 error_unsupported_cfa_register;
  u64 error_catchall;
  u64 error_should_never_happen;
  u64 error_pc_not_covered;
  u64 error_jit;
};

const volatile struct unwinder_config_t unwinder_config = {.verbose_logging =
                                                               true};

// A different stack produced the same hash.
#define STACK_COLLISION(err) (err == -EEXIST)
// Tried to read a kernel stack from a non-kernel context.
#define IN_USERSPACE(err) (err == -EFAULT)

#define LOG(fmt, ...)                                                          \
  ({                                                                           \
    if (unwinder_config.verbose_logging) {                                     \
      bpf_printk(fmt, ##__VA_ARGS__);                                          \
    }                                                                          \
  })

// Unwind tables are splitted in chunks and each chunk
// maps to a range of unwind rows within a shard.
typedef struct {
  u64 low_pc;
  u64 high_pc;
  u64 shard_index;
  u64 low_index;
  u64 high_index;
} chunk_info_t;

// Unwind table shards for an executable mapping.
typedef struct {
  chunk_info_t chunks[MAX_UNWIND_TABLE_CHUNKS];
} unwind_info_chunks_t;

// The addresses of a native stack trace.
typedef struct {
  u64 len;
  u64 addresses[MAX_STACK_DEPTH];
} stack_trace_t;

// Represents an executable mapping.
typedef struct {
  u32 executable_id;
  u32 type;
  u64 load_address;
  u64 begin;
  u64 end;
} mapping_t;

// Executable mappings for a process.
typedef struct {
  u32 is_jit_compiler;
  u32 len;
  mapping_t mappings[MAX_MAPPINGS_PER_PROCESS];
} process_info_t;

// A row in the stack unwinding table for x86_64.
typedef struct __attribute__((packed)) {
  u64 pc;
  u8 cfa_type;
  u8 rbp_type;
  u16 cfa_offset;
  s16 rbp_offset;
} stack_unwind_row_t;

_Static_assert(sizeof(stack_unwind_row_t) == 14,
               "unwind row has the expected size");

// Unwinding table representation.
typedef struct {
  stack_unwind_row_t rows[MAX_UNWIND_TABLE_SIZE];
} stack_unwind_table_t;

typedef struct {
  unsigned long long addresses[MAX_STACK_DEPTH];
  unsigned long long len;
} native_stack_t;

typedef struct {
  int task_id;
  int pid;
  int tgid;
  unsigned long long user_stack_id;
  unsigned long long kernel_stack_id;
} stack_count_key_t;

typedef struct {
  native_stack_t stack;

  unsigned long long ip;
  unsigned long long sp;
  unsigned long long bp;
  int tail_calls;

  stack_count_key_t stack_key;
} unwind_state_t;

enum event_type {
  EVENT_NEW_PROCESS = 1,
  // EVENT_NEED_UNWIND_INFO = 2, need a way to signal of new loaded mappings
};

typedef struct {
  enum event_type type;
  int pid; // use right name here (tgid?)
} Event;

enum program {
  PROGRAM_NATIVE_UNWINDER = 0,
};

#define BIG_CONSTANT(x) (x##LLU)
unsigned long long hash_stack(native_stack_t *stack) {
  const unsigned long long m = BIG_CONSTANT(0xc6a4a7935bd1e995);
  const int r = 47;
  const int seed = 123;

  unsigned long long hash = seed ^ (stack->len * m);

  for (int i = 0; i < MAX_STACK_DEPTH; i++) {
    unsigned long long k = stack->addresses[i];

    k *= m;
    k ^= k >> r;
    k *= m;

    hash ^= k;
    hash *= m;
  }

  return hash;
}