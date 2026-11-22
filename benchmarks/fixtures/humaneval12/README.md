# HumanEval-Decompile 12, O2

The MIT-licensed [dataset](https://github.com/albertan017/LLM4Decompile/blob/85b364bf093eb2ee4f3687cfe38a203fca89f23e/legacy-test/decompile-eval-executable-gcc-obj.json)
contains source, assertions and disassembly. This fixture compiles its own relocatable object from the original C function.
It includes the original assertions in `test.c`. Reproduce it from the pinned JSON on x86-64 Linux with Debian GCC 14.2.0-19 and GNU binutils 2.44.

```sh
jq -r '.[] | select(.task_id == 12 and .type == "O2") | .c_func' decompile-eval-executable-gcc-obj.json > func0.c
jq -r '.[] | select(.task_id == 12 and .type == "O2") | .c_test' decompile-eval-executable-gcc-obj.json > test.c
gcc -O2 -c func0.c -o function.o
sha256sum function.o
base64 function.o > function.o.b64
LC_ALL=C objdump -drs -j .text -j .rodata.str1.1 function.o | sed 's/[[:space:]]*$//' > listing.txt
```

The SHA-256 of the decoded ELF is `bc421cc9a074df3106e73441b609d5b1bab8e14ad809aa6db9429ebb4b53023f`.
The read-only MCP server returns the recorded disassembly after checking that the workspace contains this exact object.
The grader builds the recovered source with the **host** C compiler. This lets aarch64 and x86-64 hosts grade the same input.
