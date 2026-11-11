import type { ContainerStatus, Job, LocalStatus, ServiceStatus } from './api'

export interface Running {
  services: ServiceStatus[]
  serviceJobs: Map<number, Job>
  otherJobs: Job[]
  containers: ContainerStatus[]
  runningContainers: ContainerStatus[]
  count: number
}

export function running(local: LocalStatus | null, jobs: Job[]): Running {
  const services = (local?.services ?? []).filter((s) => s.state !== 'stopped')
  const servicePids = new Set(services.flatMap((s) => (s.pid === null ? [] : [s.pid])))
  const serviceJobs = new Map(jobs.flatMap((j) => j.roots.filter((r) => servicePids.has(r)).map((r) => [r, j] as const)))
  const otherJobs = jobs.filter((j) => !j.roots.some((r) => servicePids.has(r)))
  const containers = local?.stack ?? []
  const runningContainers = containers.filter((c) => c.state === 'running')
  return {
    services,
    serviceJobs,
    otherJobs,
    containers,
    runningContainers,
    count: services.length + otherJobs.length + (runningContainers.length ? 1 : 0),
  }
}
