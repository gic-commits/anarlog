import { CheckIcon, MicIcon } from "lucide-react";
import { useCallback, useEffect, useState } from "react";

import { commands as listenerCommands } from "@hypr/plugin-transcription";
import { Button } from "@hypr/ui/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@hypr/ui/components/ui/popover";
import { cn } from "@hypr/utils";

import { useListener } from "~/stt/contexts";

export function MicDevicePicker({ compact = false }: { compact?: boolean }) {
  const [open, setOpen] = useState(false);
  const [devices, setDevices] = useState<string[]>([]);
  const selectedMicDevice = useListener(
    (state) => state.live?.selectedMicDevice ?? null,
  );
  const currentDevice = useListener((state) => state.live?.device ?? null);
  const setSelectedMicDevice = useListener(
    (state) => state.setSelectedMicDevice,
  );

  const refreshDevices = useCallback(() => {
    const list = listenerCommands.listMicrophoneDevices;
    if (typeof list !== "function") {
      return;
    }
    void list().then((result) => {
      if (result.status === "ok") {
        setDevices(result.data);
      }
    });
  }, []);

  useEffect(() => {
    refreshDevices();
  }, [refreshDevices]);

  const activeLabel = selectedMicDevice ?? currentDevice ?? "Default";

  const choose = (device: string | null) => {
    setSelectedMicDevice(device);
    setOpen(false);
  };

  const itemClassName = cn([
    "flex w-full items-center justify-between gap-2 rounded-md px-2 py-1.5",
    "hover:bg-accent text-sm",
  ]);

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (next) {
          refreshDevices();
        }
      }}
    >
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className="gap-1.5 px-2 text-xs font-normal"
          title="Microphone"
        >
          <MicIcon className="size-4" />
          {!compact && <span className="max-w-40 truncate">{activeLabel}</span>}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-72 p-2">
        <p className="text-muted-foreground px-2 pt-1 pb-1.5 text-xs">
          Microphone
        </p>
        <button
          type="button"
          className={itemClassName}
          onClick={() => choose(null)}
        >
          <span>Follow system default</span>
          {selectedMicDevice === null && (
            <CheckIcon className="size-3.5 shrink-0" />
          )}
        </button>
        {devices.map((device) => (
          <button
            key={device}
            type="button"
            className={itemClassName}
            onClick={() => choose(device)}
          >
            <span className="truncate">{device}</span>
            {selectedMicDevice === device && (
              <CheckIcon className="size-3.5 shrink-0" />
            )}
          </button>
        ))}
      </PopoverContent>
    </Popover>
  );
}
