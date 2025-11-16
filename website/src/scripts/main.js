import '../styles/style.css';

// Register GSAP ScrollTrigger plugin
gsap.registerPlugin(ScrollTrigger);

// Performance: Set default GPU acceleration
gsap.config({ force3D: true });

// Initialize animations when DOM is loaded
document.addEventListener('DOMContentLoaded', async () => {
    // Import modules
    const { shouldShowIntro, initCinematicIntro } = await import('./intro.js');
    const { initParticles } = await import('./particles.js');
    const { initHeroAnimations } = await import('./hero.js');
    const { initScrollAnimations } = await import('./scroll-animations.js');
    const { initFeatureHovers } = await import('./features.js');
    const { initDebuggerSimulation } = await import('./debugger-simulation.js');
    const { initFloatingNavigation } = await import('./navigation.js');
    const { initDepthEnhancements } = await import('./depth-enhancements.js');
    const { initRoadmap } = await import('./roadmap.js');
    const { initParallax } = await import('./parallax.js');

    // Check if intro should be shown (24 hour expiration)
    if (shouldShowIntro()) {
        // Pass callback to initialize particles and hero after intro completes
        initCinematicIntro(() => {
            initParticles();
            initHeroAnimations();
        });
    } else {
        initParticles();
        initHeroAnimations();
    }

    // Initialize all other features
    initScrollAnimations();
    initFeatureHovers();
    initDebuggerSimulation();
    initFloatingNavigation();
    initDepthEnhancements();
    initRoadmap();
    initParallax();
});
