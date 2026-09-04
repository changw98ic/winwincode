// SPDX-License-Identifier: Apache-2.0

import { assertMounted, removeNode, type MountedView } from './mounted-view.js'

export interface FormFieldProps {
  readonly id: string
  readonly label: string
  readonly control: HTMLElement
  readonly help?: string
  readonly error?: string
  readonly required?: boolean
  readonly className?: string
}

export interface FormFieldMountOptions {
  readonly document: Document
  readonly props: Readonly<FormFieldProps>
}

export interface FormFieldView extends MountedView<FormFieldProps> {
  readonly root: HTMLDivElement
  readonly label: HTMLLabelElement
  readonly control: HTMLElement
  readonly help: HTMLParagraphElement
  readonly error: HTMLParagraphElement
}

export function mountFormField(options: FormFieldMountOptions): FormFieldView {
  const fieldId = options.props.id
  const control = options.props.control
  const root = options.document.createElement('div')
  const label = options.document.createElement('label')
  const help = options.document.createElement('p')
  const error = options.document.createElement('p')
  let open = true

  root.dataset.wwcComponent = 'form-field'
  label.className = 'wwc-form-field-label'
  help.className = 'wwc-form-field-help'
  error.className = 'wwc-form-field-error'
  control.id = `${fieldId}-control`
  label.htmlFor = control.id
  help.id = `${fieldId}-help`
  error.id = `${fieldId}-error`
  error.setAttribute('role', 'alert')
  root.append(label, control, help, error)

  function update(props: Readonly<FormFieldProps>): void {
    assertMounted(open, 'FormField')
    if (props.id !== fieldId) throw new Error('FormField id cannot change after mount.')
    if (props.control !== control) throw new Error('FormField control cannot change after mount.')
    root.className = props.className ?? 'wwc-form-field'
    label.textContent = props.label
    help.textContent = props.help ?? ''
    help.hidden = props.help === undefined
    error.textContent = props.error ?? ''
    error.hidden = props.error === undefined
    if (props.required === true) control.setAttribute('aria-required', 'true')
    else control.removeAttribute?.('aria-required')
    if (props.error === undefined) control.removeAttribute?.('aria-invalid')
    else control.setAttribute('aria-invalid', 'true')
    const descriptions = [
      ...(props.help === undefined ? [] : [help.id]),
      ...(props.error === undefined ? [] : [error.id]),
    ]
    if (descriptions.length === 0) control.removeAttribute?.('aria-describedby')
    else control.setAttribute('aria-describedby', descriptions.join(' '))
  }

  update(options.props)

  return {
    root,
    label,
    control,
    help,
    error,
    update,
    close() {
      if (!open) return
      open = false
      removeNode(root)
    },
  }
}
