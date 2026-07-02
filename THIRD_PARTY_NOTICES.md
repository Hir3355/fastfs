# Third-party notices

FastFs 0.6.2 uses the following general-purpose Rust crates. The versions are
the versions locked in `Cargo.lock`.

| Crate | Version | License used by FastFs |
| --- | ---: | --- |
| `aho-corasick` | 1.1.5 | Unlicense |
| `memchr` | 2.8.3 | Unlicense |
| `regex-automata` | 0.4.18 | MIT |
| `regex-syntax` | 0.8.11 | MIT |
| `num_cpus` | 1.17.0 | MIT |
| `windows-sys` | 0.61.2 | MIT |
| `windows-link` | 0.2.1 | MIT |
| `hermit-abi` | 0.5.2 | MIT |
| `libc` | 0.2.189 | MIT |

`windows-link`, `hermit-abi`, and `libc` are target-dependent transitive
dependencies. They are listed so this notice covers every package present in
the lock file.

## Unlicense

The following license applies to `aho-corasick` and `memchr`:

This is free and unencumbered software released into the public domain.

Anyone is free to copy, modify, publish, use, compile, sell, or distribute this
software, either in source code form or as a compiled binary, for any purpose,
commercial or non-commercial, and by any means.

In jurisdictions that recognize copyright laws, the author or authors of this
software dedicate any and all copyright interest in the software to the public
domain. We make this dedication for the benefit of the public at large and to
the detriment of our heirs and successors. We intend this dedication to be an
overt act of relinquishment in perpetuity of all present and future rights to
this software under copyright law.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN
ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

For more information, please refer to <https://unlicense.org/>.

## MIT licenses

The following copyright notices apply to the indicated packages:

- `regex-automata` and `regex-syntax`: Copyright (c) 2014 The Rust Project Developers
- `num_cpus`: Copyright (c) 2015-2025 Sean McArthur
- `windows-sys` and `windows-link`: Copyright (c) Microsoft Corporation
- `libc`: Copyright (c) The Rust Project Developers
- `hermit-abi`: its distributed MIT license contains no copyright line

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
