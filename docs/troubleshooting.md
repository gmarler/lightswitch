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
      rq_handler-67933 [016] d.h. 388101.861053: bpf_trace_printk: ## frame: 0
      rq_handler-67933 [016] d.h. 388101.861055: bpf_trace_printk: 	current pc: 7f0ef72bf928
      rq_handler-67933 [016] d.h. 388101.861056: bpf_trace_printk: 	current sp: 7edd36982440
      rq_handler-67933 [016] d.h. 388101.861056: bpf_trace_printk: 	current bp: 0
      rq_handler-67933 [016] d.h. 388101.861060: bpf_trace_printk: target 9928 left 11019 right 12172
      rq_handler-67933 [016] d.h. 388101.861062: bpf_trace_printk: 	.done
      rq_handler-67933 [016] d.h. 388101.861062: bpf_trace_printk: 	=> table_index: 11913
      rq_handler-67933 [016] d.h. 388101.861062: bpf_trace_printk: 	=> object relative pc: f9928
      rq_handler-67933 [016] d.h. 388101.861063: bpf_trace_printk: 	cfa type: 2, offset: 48 (row pc: f9914)
      rq_handler-67933 [016] d.h. 388101.861064: bpf_trace_printk: 	(bp_offset: -16, bp value stored at 7edd36982460)
      rq_handler-67933 [016] d.h. 388101.861065: bpf_trace_printk: 	previous ip: 7f0ef72ed188 (@ 7edd36982468)
      rq_handler-67933 [016] d.h. 388101.861065: bpf_trace_printk: 	previous sp: 7edd36982470
      rq_handler-67933 [016] d.h. 388101.861065: bpf_trace_printk: 	previous bp: 7edd36982520
```
