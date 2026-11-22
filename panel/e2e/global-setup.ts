import { startStack } from './stack.mjs';

export default async function globalSetup() {
  // E2E_REUSE_STACK=1: test against a stack started with `npm run stack`.
  if (process.env.E2E_REUSE_STACK) return;
  const stack = await startStack();
  return async () => { await stack.stop(); };
}
