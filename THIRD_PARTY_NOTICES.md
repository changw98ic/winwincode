# Third-party notices

WinWinCode project-owned code is licensed only under the Apache License,
Version 2.0. Third-party terms below apply only to the identified third-party
components and do not add another license to WinWinCode project-owned code.

## OpenAI Codex

WinWinCode incorporates source derived from OpenAI Codex `rust-v0.149.0`,
commit `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`. OpenAI Codex is licensed under
the Apache License, Version 2.0. The complete Apache-2.0 text is in `LICENSE`.

OpenAI Codex
Copyright 2025 OpenAI

OpenAI Codex includes code derived from Ratatui.

Copyright (c) 2016-2022 Florian Dehau
Copyright (c) 2023-2025 The Ratatui Developers

## DeepSeek Harness and Ratatui MIT terms

WinWinCode uses DeepSeek Harness packages from release `0.1.0-rc.8` and Cordis
`4.0.1`.

DeepSeek Harness
Copyright (c) 2026 DeepSeek

Ratatui portions identified above are also distributed under these MIT terms.

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

## Native Rust dependency notices

Every generated platform package contains a target-specific
`rust-dependencies.json`. It records each linked Cargo package's exact name,
version, source, authors, declared license expression, and the SHA-256 identity
of every license, notice, copying, or copyright file included by that package's
source archive. The referenced files are stored once by digest in the package's
`licenses/` directory. `build-info.json` covers the inventory and every legal
file with checksums.

When a Cargo source archive declares a license but does not contain a separate
license file, its declared expression and authors remain in the inventory. For
an `Apache-2.0 OR ...` declaration, this distribution elects the Apache-2.0
option. The Apache-2.0 text is bundled as `LICENSE`; the MIT text is reproduced
above. The one dependency in this build that declares only BSD-2-Clause and
ships no separate license file is `Inflector 0.11.4`, by Josh Teeter. Its terms
are:

Copyright (c) Josh Teeter

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.
