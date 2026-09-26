# Third-party notices

The SgCap executable contains code from the following Rust crates. Each of
them is available under the MIT license (several are dual-licensed under
MIT or Apache-2.0, or MIT, Zlib or Apache-2.0; SgCap redistributes them under
the MIT terms reproduced at the end of this file). Build-time tools that are
not part of the distributed binary (embed-manifest, syn, quote, proc-macro2,
unicode-ident, windows-implement, windows-interface) are not listed.

| Crate | Version | Copyright | Source |
|---|---|---|---|
| windows | 0.62.2 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-collections | 0.3.2 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-core | 0.62.2 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-future | 0.3.2 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-link | 0.2.1 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-numerics | 0.3.1 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-result | 0.4.1 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-strings | 0.5.1 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| windows-threading | 0.2.1 | Copyright (c) Microsoft Corporation. | https://github.com/microsoft/windows-rs |
| png | 0.18.1 | Copyright (c) 2015 nwin | https://github.com/image-rs/image-png |
| fdeflate | 0.3.7 | The image-rs developers (see LICENSE-MIT in the crate) | https://github.com/image-rs/fdeflate |
| flate2 | 1.1.10 | Copyright (c) 2014-2026 Alex Crichton | https://github.com/rust-lang/flate2-rs |
| miniz_oxide | 0.8.9, 0.9.1 | Copyright 2013-2014 RAD Game Tools and Valve Software; Copyright 2010-2014 Rich Geldreich and Tenacious Software LLC; Copyright (c) 2017 Frommi | https://github.com/Frommi/miniz_oxide |
| crc32fast | 1.5.2 | Copyright (c) 2018 Sam Rijs, Alex Crichton and contributors | https://github.com/srijs/rust-crc32fast |
| simd-adler32 | 0.3.10 | Copyright (c) 2021 Marvin Countryman | https://github.com/mcountryman/simd-adler32 |
| adler2 | 2.0.1 | The adler2 authors (see LICENSE-MIT in the crate) | https://github.com/oyvindln/adler2 |
| bitflags | 2.13.2 | Copyright (c) 2014 The Rust Project Developers | https://github.com/bitflags/bitflags |
| cfg-if | 1.0.5 | Copyright (c) 2014 Alex Crichton | https://github.com/rust-lang/cfg-if |

The executable also links dynamically against the Windows system libraries
that ship with the operating system.

## MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
