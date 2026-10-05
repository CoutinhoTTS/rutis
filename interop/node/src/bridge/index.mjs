// Bridge plugins for Cordis applications: link a Cordis app to rutis nodes
// (or other Cordis apps) as a full framework node, and choose per link what
// crosses it. See docs/design-remote-plugins-2026-10-03.md.
export { Link, peerService } from './link.mjs'
export { Export, Import, Host, Events } from './features.mjs'
