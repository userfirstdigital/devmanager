import { useAppStore } from '../../stores/appStore';
import { ProjectCard } from './ProjectCard';

export function ProjectList() {
  const config = useAppStore(s => s.config);
  if (!config) return null;

  const pinned = config.projects.filter(p => p.pinned);
  const unpinned = config.projects.filter(p => !p.pinned);

  return (
    <div className="p-2 space-y-1">
      {pinned.map(project => (
        <ProjectCard key={project.id} project={project} />
      ))}
      {pinned.length > 0 && unpinned.length > 0 && (
        <div className="border-t border-zinc-700 my-2" />
      )}
      {unpinned.map(project => (
        <ProjectCard key={project.id} project={project} />
      ))}
      {config.projects.length === 0 && (
        <p className="text-xs text-zinc-500 text-center py-4">No projects yet</p>
      )}
    </div>
  );
}
