import { useEffect } from 'react';
import { check } from '@tauri-apps/plugin-updater';

export function useUpdateCheck() {
  useEffect(() => {
    const doCheck = async () => {
      try {
        const update = await check();
        if (update) {
          window.dispatchEvent(
            new CustomEvent('devmanager-update-available', {
              detail: {
                version: update.version,
                body: update.body ?? '',
                update, // pass the update object so StatusBar can install
              },
            })
          );
        }
      } catch (err) {
        console.warn('[update-check]', err);
      }
    };

    const timeout = setTimeout(doCheck, 5000);
    return () => clearTimeout(timeout);
  }, []);
}
