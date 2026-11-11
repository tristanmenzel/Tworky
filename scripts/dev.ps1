# Runs `scripts/tauri-dev.mjs` (tauri dev) in a kill-on-close job object, so Vite, cargo and the app all stop
# when this script ends, however it ends. Windows doesn't stop children with their parent, and
# Git Bash loses parent links, so killing a launcher otherwise leaves Vite holding port 1420.
# The script also ends when its own parent does, for launchers that kill only the top process.
param([Parameter(ValueFromRemainingArguments)] [string[]] $Rest)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class KillOnCloseJob {
    [StructLayout(LayoutKind.Sequential)]
    struct BasicLimits {
        public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass, SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct ExtendedLimits {
        public BasicLimits Basic;
        public ulong ReadOps, WriteOps, OtherOps, ReadBytes, WriteBytes, OtherBytes;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr CreateJobObject(IntPtr attributes, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref ExtendedLimits info, int size);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();

    public static IntPtr JoinCurrentProcess() {
        IntPtr job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
        var info = new ExtendedLimits();
        info.Basic.LimitFlags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if (!SetInformationJobObject(job, 9, ref info, Marshal.SizeOf(info)))
            throw new System.ComponentModel.Win32Exception();
        if (!AssignProcessToJobObject(job, GetCurrentProcess()))
            throw new System.ComponentModel.Win32Exception();
        return job;
    }
}
'@

$job = [KillOnCloseJob]::JoinCurrentProcess()
$parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId

$start = [System.Diagnostics.ProcessStartInfo]::new('node', "scripts/tauri-dev.mjs $Rest")
$start.UseShellExecute = $false
$start.WorkingDirectory = Split-Path $PSScriptRoot
$dev = [System.Diagnostics.Process]::Start($start)

while (-not $dev.WaitForExit(1000)) {
    if (-not (Get-Process -Id $parent -ErrorAction SilentlyContinue)) { exit 1 }
}
exit $dev.ExitCode
