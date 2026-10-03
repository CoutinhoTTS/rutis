// The JSON encoding of protocol frames. Framing is the channel's business:
// a byte stream adds a newline, a message channel sends the text as is.
export function encode(value) {
  return JSON.stringify(value, (_key, value) => {
    if (typeof value === 'number' && !Number.isFinite(value)) throw new TypeError('non-finite number')
    return value === undefined ? null : value
  })
}

export function decode(text) {
  return JSON.parse(text)
}
