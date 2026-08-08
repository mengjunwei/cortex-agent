export function uuidv7() {
  const bytes = crypto.getRandomValues(new Uint8Array(16))
  const timestamp = Date.now()

  bytes[0] = (timestamp / 0x10000000000) & 0xff
  bytes[1] = (timestamp / 0x100000000) & 0xff
  bytes[2] = (timestamp / 0x1000000) & 0xff
  bytes[3] = (timestamp / 0x10000) & 0xff
  bytes[4] = (timestamp / 0x100) & 0xff
  bytes[5] = timestamp & 0xff
  bytes[6] = (bytes[6] & 0x0f) | 0x70
  bytes[8] = (bytes[8] & 0x3f) | 0x80

  const hex = Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}
