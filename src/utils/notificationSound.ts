// Gentle notification chime using Web Audio API — no audio file needed
let audioCtx: AudioContext | null = null;

function getAudioContext(): AudioContext {
  if (!audioCtx) {
    audioCtx = new AudioContext();
  }
  return audioCtx;
}

/** Play a soft two-tone chime to signal Claude is ready */
export function playReadyChime() {
  try {
    const ctx = getAudioContext();
    const now = ctx.currentTime;

    // Two gentle sine tones: C5 then E5
    const frequencies = [523.25, 659.25];
    const duration = 0.15;
    const gap = 0.1;

    for (let i = 0; i < frequencies.length; i++) {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();

      osc.type = 'sine';
      osc.frequency.value = frequencies[i];

      const start = now + i * (duration + gap);
      gain.gain.setValueAtTime(0, start);
      gain.gain.linearRampToValueAtTime(0.08, start + 0.02);  // soft attack
      gain.gain.linearRampToValueAtTime(0, start + duration);  // fade out

      osc.connect(gain);
      gain.connect(ctx.destination);
      osc.start(start);
      osc.stop(start + duration);
    }
  } catch {
    // Audio not available — silent fallback
  }
}
