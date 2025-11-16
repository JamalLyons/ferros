/**
 * Cinematic intro animation that plays once per 24 hours
 * @param {Function} onComplete - Callback function called after intro completes
 */

export function initCinematicIntro(onComplete) {
    const introOverlay = document.createElement('div');
    introOverlay.id = 'intro-overlay';
    introOverlay.className = 'fixed inset-0 z-[9999] bg-gradient-to-br from-[#0d0d0d] via-[#1a1a1a] to-[#0d0d0d]';
    document.body.appendChild(introOverlay);

    const spark = document.createElement('div');
    spark.id = 'intro-spark';
    spark.className = 'absolute w-2 h-2 bg-[#d3752b] rounded-full shadow-[0_0_20px_#d3752b]';
    introOverlay.appendChild(spark);

    const logoContainer = document.createElement('div');
    logoContainer.id = 'intro-logo-container';
    logoContainer.className = 'absolute inset-0 flex items-center justify-center opacity-0';
    const logo = document.createElement('img');
    logo.src = '/ferros-logo.png';
    logo.className = 'w-32 h-32 rounded-full object-cover border-2 border-[#d3752b]';
    logoContainer.appendChild(logo);
    introOverlay.appendChild(logoContainer);

    const tl = gsap.timeline();

    // Spark travels across screen
    tl.set(spark, { x: '-100vw', y: '50vh' })
        .to(spark, {
            x: '50vw',
            y: '50vh',
            duration: 1.2,
            ease: 'power2.inOut'
        })
        .to(spark, {
            scale: 8,
            opacity: 0.8,
            duration: 0.3,
            ease: 'power2.out'
        })
        // Flash effect
        .to(introOverlay, {
            backgroundColor: '#d3752b',
            duration: 0.1,
            ease: 'power2.out'
        })
        .to(introOverlay, {
            backgroundColor: '#0d0d0d',
            duration: 0.2,
            ease: 'power2.out'
        })
        // Logo appears with cooling effect
        .to(logoContainer, {
            opacity: 1,
            scale: 1,
            duration: 0.6,
            ease: 'back.out(1.4)'
        })
        .to(logo, {
            filter: 'brightness(1.2)',
            duration: 0.3
        })
        .to(logo, {
            filter: 'brightness(1)',
            duration: 0.5
        })
        // Fade out overlay
        .to(introOverlay, {
            opacity: 0,
            duration: 0.8,
            ease: 'power2.in'
        })
        .call(() => {
            // Store current timestamp for 24-hour expiration
            localStorage.setItem('ferros_intro_last_shown', Date.now().toString());
            introOverlay.remove();

            // Call completion callback if provided
            if (onComplete) {
                onComplete();
            }
        });
}

/**
 * Check if intro should be shown based on 24-hour expiration
 * @returns {boolean} True if intro should be shown
 */
export function shouldShowIntro() {
    const lastIntroTime = localStorage.getItem('ferros_intro_last_shown');
    const oneDayInMs = 24 * 60 * 60 * 1000; // 24 hours in milliseconds
    const now = Date.now();

    // Show intro if never shown before, or if 24 hours have passed
    return !lastIntroTime || (now - parseInt(lastIntroTime)) > oneDayInMs;
}

