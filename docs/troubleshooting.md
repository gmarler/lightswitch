# Troubleshooting

This document is intended to explain how to investigate issues that arise with
the lightswitch agent.

## How to Examine unwinder statistics spikes

lightswitch prints statistics for how the unwinder is working periodically.
Here's how to look into particular statistics that indicate the unwinder
may need some attention:

* error_unsupported_expression
    1. Run lightswitch with `--bpf-logging option`, so the unwinder will log
    2. In a separate shell, run `bpftool prog tracelog` and grep for "unsup", to get output like:
```
         appsock-278872 [032] d.h. 2981839.474129: bpf_trace_printk: [unsup] CFA is an unsupported expression, bailing out
           <...>-301729 [035] d.h. 2981840.842587: bpf_trace_printk: [unsup] CFA is an unsupported expression, bailing out
         appsock-237042 [027] d.h. 2981845.263661: bpf_trace_printk: [unsup] CFA is an unsupported expression, bailing out
```

* error_truncated
* error_chunk_not_found

* error_unsupported_frame_pointer_action
    1. Run lightswitch with `--bpf-logging option`, so the unwinder will log
    2. In a separate shell, run `bpftool prog tracelog` and grep for the binary
       name (in this case, rq_handler), to get output like:
```
      rq_handler-66913 [014] d.h. 59967.290608: bpf_trace_printk: target f1a0 left 68655 right 69300
      rq_handler-66913 [014] d.h. 59967.290609: bpf_trace_printk:       .done
      rq_handler-66913 [014] d.h. 59967.290609: bpf_trace_printk:       => table_index: 69211
      rq_handler-66913 [014] d.h. 59967.290609: bpf_trace_printk:       => object relative pc: 95f1a0
      rq_handler-66913 [014] d.h. 59967.290610: bpf_trace_printk:       cfa type: 1, offset: 16 (row pc: 95f15d)
      rq_handler-66913 [014] d.h. 59967.290610: bpf_trace_printk:       (bp_offset: -16, bp value stored at 7ecc0101a550)
      rq_handler-66913 [014] d.h. 59967.290611: bpf_trace_printk:       previous ip: 8f4193 (@ 7ecc0101a558)
      rq_handler-66913 [014] d.h. 59967.290611: bpf_trace_printk:       previous sp: 7ecc0101a560
      rq_handler-66913 [014] d.h. 59967.290611: bpf_trace_printk:       previous bp: 7ecc0101ad10
      rq_handler-66913 [014] d.h. 59967.290612: bpf_trace_printk: ## frame: 2
      rq_handler-66913 [014] d.h. 59967.290614: bpf_trace_printk:       current pc: 8f4193
      rq_handler-66913 [014] d.h. 59967.290615: bpf_trace_printk:       current sp: 7ecc0101a560
      rq_handler-66913 [014] d.h. 59967.290616: bpf_trace_printk:       current bp: 7ecc0101ad10
      rq_handler-66913 [014] d.h. 59967.290617: bpf_trace_printk: target 4193 left 65467 right 66003
      rq_handler-66913 [014] d.h. 59967.290619: bpf_trace_printk:       .done
      rq_handler-66913 [014] d.h. 59967.290620: bpf_trace_printk:       => table_index: 65515
      rq_handler-66913 [014] d.h. 59967.290621: bpf_trace_printk:       => object relative pc: 8f4193
      rq_handler-66913 [014] d.h. 59967.290623: bpf_trace_printk:       cfa type: 3, offset: 0 (row pc: 8f397b)
      rq_handler-66913 [014] d.h. 59967.290623: bpf_trace_printk:       [error] frame pointer is 3 (register or exp), bailing out
```
    3. Check to see what the interpreted CFA for this process at the rip
       (row pc) mentioned at 0x8f397b for the rq_handler binary:
       ```
        readelf --debug-dump=frames-interp /bb/sys/exe/rq_handler | less
       ```
        and search for the block containing the rip address 8f397b, which in
        this case is:
        ```
        0004b2c8 0000000000000058 0004b088 FDE cie=00000244 pc=00000000008f3960..00000000008f46f5
           LOC           CFA      rbx   rbp   r12   r13   r14   r15   ra
        00000000008f3960 rsp+8    u     u     u     u     u     u     c-8
        00000000008f3965 r10+0    u     u     u     u     u     u     c-8
        00000000008f396e r10+0    u     exp   u     u     u     u     c-8
        00000000008f397b exp      u     exp   exp   exp   exp   exp   c-8
        00000000008f397c exp      exp   exp   exp   exp   exp   exp   c-8
        00000000008f401e exp      exp   exp   exp   exp   exp   exp   c-8
        00000000008f402e exp      exp   exp   exp   exp   exp   exp   c-8
        00000000008f44ad exp      exp   exp   exp   exp   exp   exp   c-8
        00000000008f44c2 exp      exp   exp   exp   exp   exp   exp   c-8
        00000000008f4549 r10+0    exp   exp   exp   exp   exp   exp   c-8
        00000000008f4556 rsp+8    exp   exp   exp   exp   exp   exp   c-8
        00000000008f4560 exp      exp   exp   exp   exp   exp   exp   c-8
        ```
        This confirms that address 8f397b is indeed a CFA expression.
    4. Now that we know we're looking for a CFA expression, we print out the
       raw CFA opcodes with:
       ```
       readelf --debug-dump=frames /bb/sys/exe/rq_handler | less
       ```
       Then we search for the block containing address 8f397b, which results in:
       ```
       0004b2c8 0000000000000058 0004b088 FDE cie=00000244 pc=00000000008f3960..00000000008f46f5
         Augmentation data:     fb 7f 3e 00
         DW_CFA_advance_loc: 5 to 00000000008f3965
         DW_CFA_def_cfa: r10 (r10) ofs 0
         DW_CFA_advance_loc: 9 to 00000000008f396e
         DW_CFA_expression: r6 (rbp) (DW_OP_breg6 (rbp): 0)
         DW_CFA_advance_loc: 13 to 00000000008f397b
         DW_CFA_def_cfa_expression (DW_OP_breg6 (rbp): -40; DW_OP_deref)
         DW_CFA_expression: r15 (r15) (DW_OP_breg6 (rbp): -8)
         DW_CFA_expression: r14 (r14) (DW_OP_breg6 (rbp): -16)
         DW_CFA_expression: r13 (r13) (DW_OP_breg6 (rbp): -24)
         DW_CFA_expression: r12 (r12) (DW_OP_breg6 (rbp): -32)
         DW_CFA_advance_loc: 1 to 00000000008f397c
         DW_CFA_expression: r3 (rbx) (DW_OP_breg6 (rbp): -48)
         DW_CFA_advance_loc2: 1698 to 00000000008f401e
         DW_CFA_GNU_args_size: 16
         DW_CFA_advance_loc: 16 to 00000000008f402e
         DW_CFA_GNU_args_size: 0
         DW_CFA_advance_loc2: 1151 to 00000000008f44ad
         DW_CFA_GNU_args_size: 16
         DW_CFA_advance_loc: 21 to 00000000008f44c2
         DW_CFA_GNU_args_size: 0
         DW_CFA_advance_loc1: 135 to 00000000008f4549
         DW_CFA_remember_state
         DW_CFA_def_cfa: r10 (r10) ofs 0
         DW_CFA_advance_loc: 13 to 00000000008f4556
         DW_CFA_def_cfa: r7 (rsp) ofs 8
         DW_CFA_advance_loc: 10 to 00000000008f4560
         DW_CFA_restore_state
         DW_CFA_nop
       ```
       This is the data needed to determine what CFA opcodes we need to add support for.
