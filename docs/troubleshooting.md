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

