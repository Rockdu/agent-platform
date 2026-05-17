// Compile-time probe for the ide-handoff wire shape (Round 32 /
// task19 / AC-4.6). Mirrors src-tauri/src/ide_handoff.rs::
// {IdePreference, IdeHandoffErrorDto}.

import {
  isIdeHandoffErrorDto,
  type IdeHandoffErrorDto,
  type IdePreference,
} from "../src/ide-handoff";

export function _probe_preference_shape(p: IdePreference): string {
  return `${p.ideCommand}|${p.ideArgsTemplate.join(",")}`;
}

export function _probe_error_dto_discrimination(e: IdeHandoffErrorDto): string {
  switch (e.kind) {
    case "ideNotInPath":
      return `ideNotInPath:${e.command}`;
    case "notADirectory":
      return `notADirectory:${e.path}`;
    case "spawnFailed":
      return `spawnFailed:${e.command}:${e.message}`;
    case "io":
      return `io:${e.context}:${e.message}`;
  }
}

export function _probe_is_ide_handoff_error_dto_narrows(): void {
  const v: unknown = { kind: "ideNotInPath", command: "cursor" };
  if (isIdeHandoffErrorDto(v)) {
    void v.kind;
  }
}

export function _probe_invalid_preference_missing_args_template(): void {
  // @ts-expect-error `ideArgsTemplate` is required on IdePreference
  const p: IdePreference = {
    ideCommand: "cursor",
  };
  void p;
}

export function _probe_invalid_error_dto_kind(): void {
  const e: IdeHandoffErrorDto = {
    // @ts-expect-error kind must be one of the named IdeHandoffErrorDto variants
    kind: "not-a-real-kind",
    message: "bogus",
  };
  void e;
}
