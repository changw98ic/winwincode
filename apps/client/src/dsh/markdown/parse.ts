import type { Root } from 'mdast'
import { fromMarkdown } from 'mdast-util-from-markdown'
import { gfmFromMarkdown } from 'mdast-util-gfm'
import { gfm } from 'micromark-extension-gfm'
import { cjkFriendlyStrong } from './cjkFriendlyStrong.js'

export function parseGfm(text: string): Root {
  return fromMarkdown(text, {
    extensions: [gfm(), cjkFriendlyStrong()],
    mdastExtensions: [gfmFromMarkdown()],
  })
}

