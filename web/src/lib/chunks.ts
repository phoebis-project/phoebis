/** The tab's reload and session storage, read when a load settles, so a bundle without a DOM can import this. */
export interface ReloadEnv {
  reload: () => void;
  storage: () => Pick<Storage, 'getItem' | 'removeItem' | 'setItem'>;
}

const browser: ReloadEnv = { reload: () => window.location.reload(), storage: () => window.sessionStorage };

const RELOADED = 'phoebis.chunkReload';

/**
 * Wraps a page chunk's import. An upgrade renames the chunks, so a tab opened before it asks for files the server no
 * longer has; the console reloads once to fetch the current entry. The mark in session storage stops a chunk that is
 * missing after the reload too from reloading for ever: that failure is answered as the error it is.
 */
export function reloadOnChunkFailure<T>(load: () => Promise<T>, env: ReloadEnv = browser): () => Promise<T> {
  return () =>
    load().then(
      (module) => {
        try {
          env.storage().removeItem(RELOADED);
        } catch {
          // Without storage there is no mark to clear.
        }
        return module;
      },
      (error: unknown) => {
        try {
          const storage = env.storage();
          if (storage.getItem(RELOADED) === null) {
            storage.setItem(RELOADED, '1');
            env.reload();
            // The fallback stays drawn until the reload replaces the page.
            return new Promise<T>(() => {});
          }
        } catch {
          // A tab without storage cannot mark a reload, so it never reloads.
        }
        throw error;
      },
    );
}
