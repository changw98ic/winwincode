// SPDX-License-Identifier: Apache-2.0

import { assertMounted } from './mounted-view.js'

export type KeyedCollectionKey = string | number

export interface KeyedCollectionMountOptions<
  Item,
  Key extends KeyedCollectionKey,
  Node extends globalThis.Node & ChildNode,
> {
  readonly parent: HTMLElement
  readonly key: (item: Item) => Key
  readonly create: (item: Item) => Node
  readonly update: (node: Node, item: Item) => void
  readonly remove?: (node: Node) => void
}

export interface KeyedCollectionView<
  Item,
  Key extends KeyedCollectionKey,
  Node extends globalThis.Node & ChildNode,
> {
  readonly root: HTMLElement
  update(items: readonly Item[]): void
  node(key: Key): Node | null
  close(): void
}

/** Maintain one bounded DOM node per stable business identity. */
export function mountKeyedCollection<
  Item,
  Key extends KeyedCollectionKey,
  Node extends globalThis.Node & ChildNode,
>(
  options: KeyedCollectionMountOptions<Item, Key, Node>,
): KeyedCollectionView<Item, Key, Node> {
  const mounted = new Map<Key, Node>()
  let open = true

  function remove(node: Node): void {
    options.remove?.(node)
    node.remove()
  }

  function update(items: readonly Item[]): void {
    assertMounted(open, 'KeyedCollection')
    const keys = items.map(options.key)
    if (new Set(keys).size !== keys.length) {
      throw new Error('KeyedCollection requires one unique key per item.')
    }
    const retained = new Set(keys)
    for (const [key, node] of mounted) {
      if (retained.has(key)) continue
      remove(node)
      mounted.delete(key)
    }

    items.forEach((item, index) => {
      const key = keys[index]
      if (key === undefined) return
      const existing = mounted.get(key)
      const node = existing ?? options.create(item)
      if (existing === undefined) mounted.set(key, node)
      options.update(node, item)
      const current = options.parent.childNodes[index] ?? null
      if (current !== node) options.parent.insertBefore(node, current)
    })
  }

  return {
    root: options.parent,
    update,
    node(key) {
      assertMounted(open, 'KeyedCollection')
      return mounted.get(key) ?? null
    },
    close() {
      if (!open) return
      open = false
      for (const node of mounted.values()) remove(node)
      mounted.clear()
    },
  }
}
