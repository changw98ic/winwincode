// SPDX-License-Identifier: Apache-2.0

export interface MountedView<Props> {
  readonly root: HTMLElement
  update(props: Readonly<Props>): void
  close(): void
}

export function assertMounted(open: boolean, name: string): void {
  if (!open) throw new Error(`${name} is closed.`)
}

export function removeNode(node: HTMLElement): void {
  node.remove?.()
}
