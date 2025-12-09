#!/usr/bin/env node
// Simple Node.js tester that forwards args to the vane-cli binary in this workspace.
// Usage: node tools/vane-tester.js --input ./path/to/bin [--jit true|false] [other flags]

const { spawn } = require('child_process');
const path = require('path');

function main() {
  const args = process.argv.slice(2);
  if (args.length === 0) {
    console.error('Usage: vane-tester.js <flags...>');
    process.exit(2);
  }

  // Build command: cargo run -p vane-cli -- <args...>
  const cargoArgs = ['run', '-p', 'vane-cli', '--'].concat(args);
  const p = spawn('cargo', cargoArgs, { stdio: 'inherit' });

  p.on('close', (code) => {
    process.exit(code);
  });
}

main();
