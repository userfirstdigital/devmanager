import { Type } from 'lucide-react';

interface FontSizeSliderProps {
  value: number;
  onChange: (size: number) => void;
}

export function FontSizeSlider({ value, onChange }: FontSizeSliderProps) {
  return (
    <div className="flex items-center gap-1.5 text-zinc-500">
      <Type size={12} />
      <input
        type="range"
        min={8}
        max={24}
        step={1}
        value={value}
        onChange={e => onChange(parseInt(e.target.value, 10))}
        className="w-16 h-1 accent-indigo-500"
        title={`Font size: ${value}px`}
      />
      <span className="text-[10px] w-5 text-right tabular-nums">{value}</span>
    </div>
  );
}
