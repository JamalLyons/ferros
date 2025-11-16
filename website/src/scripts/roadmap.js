/**
 * Roadmap visualization
 * Animates timeline and milestones on scroll
 */

export function initRoadmap() {
    const roadmapSection = document.getElementById('roadmap');
    if (!roadmapSection) return;

    const roadmapTitle = document.getElementById('roadmap-title');
    if (roadmapTitle) {
        gsap.fromTo(roadmapTitle,
            { opacity: 0, y: 30 },
            {
                opacity: 1,
                y: 0,
                duration: 0.6,
                ease: 'power4.out',
                force3D: true,
                scrollTrigger: {
                    trigger: roadmapTitle,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    const timeline = roadmapSection.querySelector('.roadmap-timeline');
    const milestones = roadmapSection.querySelectorAll('.roadmap-milestone');

    // Animate timeline on scroll
    if (timeline) {
        gsap.fromTo(timeline,
            { scaleX: 0 },
            {
                scaleX: 1,
                duration: 1,
                ease: 'power3.out',
                force3D: true,
                scrollTrigger: {
                    trigger: roadmapSection,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                }
            }
        );
    }

    // Animate milestones
    milestones.forEach((milestone, index) => {
        gsap.fromTo(milestone,
            { opacity: 0, scale: 0 },
            {
                opacity: 1,
                scale: 1,
                duration: 0.5,
                ease: 'back.out(1.4)',
                force3D: true,
                scrollTrigger: {
                    trigger: milestone,
                    start: 'top 90%',
                    toggleActions: 'play none none none'
                },
                delay: index * 0.05
            }
        );

        // Pulsing animation for active milestones
        const node = milestone.querySelector('.milestone-node');
        if (node && node.classList.contains('bg-[#d3752b]')) {
            gsap.to(node, {
                scale: 1.2,
                duration: 1.5,
                repeat: -1,
                yoyo: true,
                ease: 'sine.inOut',
                force3D: true
            });
        }
    });
}

