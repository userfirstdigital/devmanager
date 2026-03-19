export const NOTIFICATION_SOUNDS = [
  { id: 'glass', label: 'Glass' },
  { id: 'chord', label: 'Chord' },
  { id: 'glisten', label: 'Glisten' },
  { id: 'polite', label: 'Polite' },
  { id: 'calm', label: 'Calm' },
  { id: 'sharp', label: 'Sharp' },
  { id: 'jinja', label: 'Jinja' },
  { id: 'cloud', label: 'Cloud' },
  { id: 'none', label: 'None (silent)' },
];

// Cache Audio objects for instant playback
const audioCache = new Map<string, HTMLAudioElement>();

export function playNotificationSound(soundId: string) {
  if (soundId === 'none') return;
  const src = `/sounds/${soundId}.wav`;
  let audio = audioCache.get(soundId);
  if (!audio) {
    audio = new Audio(src);
    audioCache.set(soundId, audio);
  }
  const clone = audio.cloneNode() as HTMLAudioElement;
  clone.volume = 0.5;
  clone.play().catch(() => {});
}
