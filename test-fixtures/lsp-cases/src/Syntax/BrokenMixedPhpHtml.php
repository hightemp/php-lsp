<?php

declare(strict_types=1);

namespace App\Syntax;

// CASE: Mixed PHP + HTML with syntax break to test parser recovery.
echo '<div>';
?>
<section>
    <h1>Fixture</h1>
    <?php
    // ERROR: Unexpected token + missing expression.
    $x = 1 + ;
    ?>
</section>
<?php
echo '</div>';

