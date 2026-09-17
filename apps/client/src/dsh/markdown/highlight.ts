// SPDX-License-Identifier: Apache-2.0
// Adapted from DeepSeek Harness (c) 2026 DeepSeek, MIT; see THIRD_PARTY_NOTICES.md.

/**
 * The client's ONE syntax highlighter: a synchronous fine-grained shiki core
 * (JavaScript regex engine — no oniguruma WASM, bundle-friendly) with an
 * explicit grammar allowlist and a CSS-variables theme. Colors live in the
 * theme package's token sheets as `--shiki-*` custom properties (light and
 * dark blocks), never here — the repo's tokens-only styling rule.
 *
 * Only the three markdown-fence and `run_code` grammars (TypeScript, shell,
 * JSON) load into the singleton at boot — the set every session renders. The
 * read card's wider extension set (the file-extension language hints the read
 * tool's `langFromPath` emits — `packages/fs/tool-fs`: python, rust, yaml,
 * markup, …) is imported lazily and registered the first time such a language
 * is requested, so a session that never opens a read card in one of those
 * languages pays neither the ~1.6 MB of grammar modules nor their synchronous
 * init. The first render of a lazy language falls back to plain text while its
 * grammar loads, then {@link onGrammarLoaded} notifies subscribers to re-render
 * with highlighting. An unknown or absent language falls back to plain text (no
 * highlighting, still monospace) — never an error.
 */

import { createHighlighterCoreSync, createCssVariablesTheme } from 'shiki/core'
import { createJavaScriptRegexEngine, defaultJavaScriptRegexConstructor } from 'shiki/engine/javascript'
import langTs from '@shikijs/langs/typescript'
import langBash from '@shikijs/langs/shellscript'
import langJson from '@shikijs/langs/json'
import type { HighlighterCore } from 'shiki/core'

/** A shiki grammar module's default export (a `LanguageRegistration[]`), taken
 *  from a boot grammar so no direct `@shikijs/types` dependency is needed. */
type LangModule = { default: typeof langTs }

/**
 * Grammars the singleton loads at boot; each entry's own `name` is the id
 * `codeToTokens`/`codeToHtml` resolve. The JS-family aliases (js/jsx/ts/tsx)
 * resolve to the TypeScript grammar rather than a separate one: it tokenizes
 * plain TS/JS exactly, and JSX/TSX approximately (shiki's TS grammar is not the
 * dedicated TSX grammar, so JSX elements tokenize imperfectly) — an accepted
 * trade to keep the boot set to one JS-family grammar. The read card's wider
 * set loads lazily through {@link LAZY_GRAMMARS}.
 */
const LANGS = [langTs, langBash, langJson]

/**
 * The read card's extension grammars, each behind a dynamic import so its
 * module stays out of the boot chunk until a read of that language renders.
 * Keyed by the grammar id (`LanguageRegistration.name`) the aliases resolve to.
 * `@shikijs/langs`' default export is a `LanguageRegistration[]`; the loader
 * hands the whole array to `loadLanguageSync`, which registers each entry
 * (including embedded sub-grammars). The three boot grammars are absent —
 * already loaded, so no alias value ever points at a missing entry here.
 */
const LAZY_GRAMMARS = new Map<string, () => Promise<LangModule>>([
  ['python', () => import('@shikijs/langs/python')],
  ['ruby', () => import('@shikijs/langs/ruby')],
  ['go', () => import('@shikijs/langs/go')],
  ['rust', () => import('@shikijs/langs/rust')],
  ['java', () => import('@shikijs/langs/java')],
  ['c', () => import('@shikijs/langs/c')],
  ['cpp', () => import('@shikijs/langs/cpp')],
  ['csharp', () => import('@shikijs/langs/csharp')],
  ['kotlin', () => import('@shikijs/langs/kotlin')],
  ['swift', () => import('@shikijs/langs/swift')],
  ['php', () => import('@shikijs/langs/php')],
  ['yaml', () => import('@shikijs/langs/yaml')],
  ['toml', () => import('@shikijs/langs/toml')],
  ['ini', () => import('@shikijs/langs/ini')],
  ['markdown', () => import('@shikijs/langs/markdown')],
  ['html', () => import('@shikijs/langs/html')],
  ['css', () => import('@shikijs/langs/css')],
  ['scss', () => import('@shikijs/langs/scss')],
  ['less', () => import('@shikijs/langs/less')],
  ['sql', () => import('@shikijs/langs/sql')],
  ['xml', () => import('@shikijs/langs/xml')],
  ['lua', () => import('@shikijs/langs/lua')],
])

/**
 * Language ids (and aliases) the highlighter accepts; everything else renders
 * plain. A Map, not an object: fence info strings are assistant-authored, so
 * a label like `constructor` or `__proto__` must miss instead of resolving an
 * inherited property and crashing the renderer inside shiki. Keys cover both
 * the markdown-fence aliases `CodeBlock` uses and the file-extension hint ids
 * the read tool's `langFromPath` emits, so both callers resolve the same
 * grammars. The JS family maps to the TypeScript grammar (see {@link LANGS} for
 * the JSX/TSX approximation). A value not in {@link LANGS} names a
 * {@link LAZY_GRAMMARS} entry loaded on first use.
 */
const LANG_ALIASES = new Map<string, string>([
  ['typescript', 'typescript'],
  ['ts', 'typescript'],
  ['tsx', 'typescript'],
  ['javascript', 'typescript'],
  ['js', 'typescript'],
  ['jsx', 'typescript'],
  ['shellscript', 'shellscript'],
  ['bash', 'shellscript'],
  ['sh', 'shellscript'],
  ['shell', 'shellscript'],
  ['zsh', 'shellscript'],
  ['json', 'json'],
  ['jsonc', 'json'],
  ['py', 'python'],
  ['python', 'python'],
  ['rb', 'ruby'],
  ['ruby', 'ruby'],
  ['go', 'go'],
  ['rs', 'rust'],
  ['rust', 'rust'],
  ['java', 'java'],
  ['c', 'c'],
  ['cpp', 'cpp'],
  ['cs', 'csharp'],
  ['csharp', 'csharp'],
  ['kotlin', 'kotlin'],
  ['swift', 'swift'],
  ['php', 'php'],
  ['yaml', 'yaml'],
  ['yml', 'yaml'],
  ['toml', 'toml'],
  ['ini', 'ini'],
  ['md', 'markdown'],
  ['markdown', 'markdown'],
  ['html', 'html'],
  ['css', 'css'],
  ['scss', 'scss'],
  ['less', 'less'],
  ['sql', 'sql'],
  ['xml', 'xml'],
  ['lua', 'lua'],
])

/** All token colors resolve through `--shiki-*` custom properties (theme package sheets). */
const cssVariablesTheme = createCssVariablesTheme({
  name: 'css-variables',
  variablePrefix: '--shiki-',
  fontStyle: false,
})

/**
 * The client regex engine compiles each TextMate pattern when its scanner is
 * created. Shiki otherwise defers patterns longer than 3,000 characters until
 * their first match; that compilation counts against Shiki's 500 ms per-line
 * budget and can return a partial token stream under host contention. Eager
 * compilation leaves the same budget in place for scanning user content.
 */
const regexEngine = createJavaScriptRegexEngine({
  forgiving: true,
  regexConstructor: pattern => defaultJavaScriptRegexConstructor(pattern, {
    lazyCompileLength: Number.POSITIVE_INFINITY,
  }),
})

let singleton: HighlighterCore | undefined

/** Representative paths through every boot grammar, compiled before user content is timed. */
const BOOT_GRAMMAR_WARMUPS = [
  { lang: 'typescript', code: 'const answer: number = 42' },
  { lang: 'shellscript', code: 'printf \'%s\\n\' "$HOME"' },
  { lang: 'json', code: '{"ready":true}' },
] as const

/** Construct and pre-tokenize the boot grammars outside the user-content scan budget. */
function createHighlighter(): HighlighterCore {
  const instance = createHighlighterCoreSync({
    themes: [cssVariablesTheme],
    langs: LANGS,
    engine: regexEngine,
  })
  for (const sample of BOOT_GRAMMAR_WARMUPS) {
    instance.codeToTokens(sample.code, {
      lang: sample.lang,
      theme: 'css-variables',
      tokenizeTimeLimit: 0,
    })
  }
  return instance
}

/** The synchronous highlighter (one instance per document); pre-warmed below, lazy as the fallback. */
function highlighter(): HighlighterCore {
  singleton ??= createHighlighter()
  return singleton
}

/** Grammar ids whose lazy import is in flight or done, so it is requested once. */
const requested = new Set<string>()
/** Subscribers re-rendered after a lazy grammar registers (React callers). */
const listeners = new Set<() => void>()
/** Bumped on each lazy-grammar load; the `useSyncExternalStore` snapshot. */
let loadCount = 0

/**
 * Subscribe to lazy-grammar load completions; `listener` fires after a
 * {@link LAZY_GRAMMARS} grammar finishes registering on the singleton, so a
 * caller that rendered its plain fallback while the grammar loaded can
 * re-highlight. Uses the `useSyncExternalStore` subscribe signature; pair it with
 * {@link grammarLoadCount} as the snapshot. Returns an unsubscribe function.
 * @param listener - invoked (no args) on each grammar-load completion.
 * @returns a disposer that removes the listener.
 */
export function subscribeGrammarLoaded(listener: () => void): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

/**
 * The lazy-grammar load counter — a value that changes on every load, so a
 * `useSyncExternalStore` snapshot re-renders the subscriber when a grammar
 * registers. Opaque: only its identity across renders matters.
 * @returns the current load count.
 */
export function grammarLoadCount(): number {
  return loadCount
}

/**
 * Ensure the grammar `resolved` names is registered. A boot grammar (not in
 * {@link LAZY_GRAMMARS}) and an already-loaded lazy grammar report ready
 * synchronously; a lazy grammar not yet loaded starts its import (once) and
 * reports not-ready, so the caller renders plain until a
 * {@link subscribeGrammarLoaded} listener fires.
 * @param resolved - the grammar id an alias resolved to.
 * @returns whether the grammar is registered and ready to tokenize now.
 */
function ensureGrammar(resolved: string): boolean {
  const load = LAZY_GRAMMARS.get(resolved)
  // A boot grammar (already registered) has no lazy loader; it is always ready.
  if (load === undefined) return true
  if (highlighter().getLoadedLanguages().includes(resolved)) return true
  if (!requested.has(resolved)) {
    requested.add(resolved)
    void load().then((mod) => {
      highlighter().loadLanguageSync(mod.default)
      loadCount += 1
      for (const listener of listeners) listener()
    })
  }
  return false
}

// Engine + grammar construction costs a long task (~120-175ms); building it
// during the first finalized fence's render would jank exactly when a stream
// completes. Warm the singleton in a deferred task at module load (= plugin
// boot) instead; the lazy path above stays as the correctness fallback for a
// fence that renders before the timer fires. `unref` (Node-only) keeps a
// non-browser import from pinning the event loop.
const warmupTimer = setTimeout(() => { highlighter() }, 0)
;(warmupTimer as { unref?: () => void }).unref?.()

/**
 * Highlight `code` into shiki's HTML (a single `<pre class="shiki">` tree)
 * when `lang` maps to a registered grammar; `undefined` means the caller
 * renders its plain fallback. A lazy grammar not yet loaded returns `undefined`
 * for this call and loads in the background; subscribe with
 * {@link onGrammarLoaded} to re-highlight once it registers.
 * @param code - the source text.
 * @param lang - the language hint (a markdown fence info string or a fixed caller id).
 * @returns the highlighted HTML, or `undefined` for unknown or not-yet-loaded languages.
 */
export function highlightToHtml(code: string, lang: string | undefined): string | undefined {
  const resolved = lang === undefined ? undefined : LANG_ALIASES.get(lang.toLowerCase())
  if (resolved === undefined) return undefined
  if (!ensureGrammar(resolved)) return undefined
  return highlighter().codeToHtml(code, {
    lang: resolved,
    theme: 'css-variables',
    transformers: [{
      // The app's CSP permits stylesheet classes, not inline styles.
      pre(node) { delete node.properties.style },
      span(node) {
        const color = String(node.properties.style ?? '').match(/color:var\(--shiki-([a-z-]+)\)/u)?.[1]
        if (color !== undefined) this.addClassToHast(node, `wwc-syntax-${color}`)
        delete node.properties.style
      },
    }],
  })
}

