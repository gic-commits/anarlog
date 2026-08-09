import { useListener } from "~/stt/contexts";

export function RecordingTimer() {
  const status = useListener((state) => state.live.status);
  const seconds = useListener((state) => state.live.seconds);

  if (status !== "active" || seconds <= 0) {
    return null;
  }

  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;
  const label =
    hours > 0
      ? `${hours}:${String(minutes).padStart(2, "0")}:${String(secs).padStart(2, "0")}`
      : `${minutes}:${String(secs).padStart(2, "0")}`;

  return (
    <span
      className="text-muted-foreground font-mono text-xs tabular-nums"
      aria-label="Recording duration"
    >
      {label}
    </span>
  );
}
