// Why a channel could not be established. The category decides what the
// dialing side does next; `reason` is for diagnostics only.
export class ConnectError extends Error {
  // 'retryable' | 'auth-rejected' | 'incompatible'
  constructor(category, reason) {
    super(reason)
    this.name = 'ConnectError'
    this.category = category
  }
}
