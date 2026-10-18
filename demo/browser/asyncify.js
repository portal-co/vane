// Binaryen asyncify driver for vane (non-local hosts, PLAN §4.2).
//
// The Rust interpreter compiled to wasm32 is a coroutine: a host import whose
// result isn't locally ready (a non-local host's round-trip — e.g. remote
// browser memory via wsdom) must suspend the guest. Two wasm-level suspension
// mechanisms exist, interchangeable from the guest's point of view:
//
//   - JSPI (`WebAssembly.Suspending`/`promising`): zero-overhead, needs a
//     JSPI-capable engine.
//   - Binaryen asyncify (`wasm-opt --asyncify`): portable — a shadow stack
//     lets an async import unwind the guest and a later rewind resume it.
//
// This module is the asyncify driver. The async host imports call
// `Asyncify.hostCall(roundTrip)`; when the guest is running normally that
// unwinds it and kicks off the round-trip, and on the rewind the same import
// returns the resolved value.

export const ASYNCIFY_NONE = 0;
export const ASYNCIFY_UNWIND = 1;
export const ASYNCIFY_REWIND = 2;

export class Asyncify {
  /**
   * @param {WebAssembly.Exports} exports - exports of a wasm module built
   *   with `wasm-opt --asyncify --pass-arg asyncify-imports=...`.
   * @param {number} dataAddr - guest address of the asyncify shadow-stack
   *   data region (8-byte header + stack space).
   * @param {number} stackSize - shadow-stack size in bytes.
   */
  constructor(exports, dataAddr, stackSize) {
    this.exports = exports;
    this.dataAddr = dataAddr;
    this.result = undefined;
    this._wake = null;
    // Initialize the shadow-stack bounds header.
    const view = new DataView(exports.memory.buffer);
    view.setInt32(dataAddr, dataAddr + 8, true); // stack start
    view.setInt32(dataAddr + 4, dataAddr + 8 + stackSize, true); // stack end
  }

  state() {
    return this.exports.asyncify_get_state();
  }

  /**
   * Call from an async host import. On the normal (unwinding) pass it starts
   * the round-trip and unwinds the guest; on the rewinding pass it returns
   * the round-trip's resolved value.
   * @template T
   * @param {() => Promise<T>} roundTrip
   * @returns {T}
   */
  hostCall(roundTrip) {
    if (this.state() === ASYNCIFY_NONE) {
      // Begin unwind; the guest suspends at this import.
      this.exports.asyncify_start_unwind(this.dataAddr);
      roundTrip().then((value) => {
        this.result = value;
        if (this._wake) this._wake();
      });
      // Return value is discarded during the unwind.
      return /** @type {T} */ (undefined);
    }
    // Rewinding: deliver the resolved round-trip value to the guest.
    return /** @type {T} */ (this.result);
  }

  /**
   * Run an asyncify-instrumented export to completion, suspending at async
   * host imports and resuming when each round-trip resolves. Re-invoking
   * `entry` after a rewind continues the guest from the suspension point.
   * @template T
   * @param {(...args: any[]) => T} entry
   * @param  {...any} args
   * @returns {Promise<T>}
   */
  async run(entry, ...args) {
    let ret = entry(...args);
    while (this.state() === ASYNCIFY_UNWIND) {
      this.exports.asyncify_stop_unwind();
      // Wait for the current round-trip to resolve.
      await new Promise((resolve) => {
        this._wake = resolve;
      });
      this._wake = null;
      // Rewind: re-enter the export; the guest continues from where it
      // suspended, and the host import returns the round-trip value.
      this.exports.asyncify_start_rewind(this.dataAddr);
      ret = entry(...args);
      this.exports.asyncify_stop_rewind();
    }
    return ret;
  }
}
